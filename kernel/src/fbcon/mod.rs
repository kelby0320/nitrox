//! The framebuffer console: what COM1 receives, drawn on the screen, for a machine with no
//! serial port.
//!
//! `kprint` writes to COM1 and tees into the log ring, and `/dev/log` can only be read once
//! userspace runs. The laptop Phase 5 targets has no serial port at all, so a boot that died
//! before `init` used to be a black screen. This draws the same byte stream — the kernel's own
//! output and every `sys_kprint` — onto Limine's framebuffer from the first line of
//! `kernel_main`, before ACPI, PCI or anything else that can fail.
//!
//! What it shows is decided in [`text`] and drawn with [`glyphs`]; both are free of the kernel so
//! the gate that reads the screen back (`cargo xtask check-fbcon`) compiles the same code.
//!
//! ## Who owns the screen
//!
//! - **The kernel, from boot.** Every write is on the screen before the call that made it
//!   returns, so the last line a hung boot printed is the last line visible.
//! - **Userspace, from the first `/dev/framebuffer` handout** ([`yield_to_userspace`]). The
//!   kernel cannot see a frame committed into an aperture mapped straight into a client, but it
//!   does see the handout, and it happens under this console's lock — so no paint is in flight
//!   by the time a client holds the handle. Writes still land in the text grid; none is drawn.
//! - **The kernel again when the machine stops** ([`reclaim_for_stop`], called by
//!   `stop_the_machine` for a panic and a fatal fault alike). The screen is repainted from the
//!   grid, whose last rows are the diagnosis just written.
//! - **The hardware report, before userspace, on a boot that asked for one** ([`hold_for_report`]
//!   to [`end_report`], Phase 5 Part D.3). Writes still land in the grid and none is drawn: the
//!   screen shows the page the report put there, so a line printed while a person reads it — an
//!   AP announcing itself late — cannot scroll the page's top rows away. The end repaints the grid.
//!
//! ## Locking, and the three ways a lock could hang the path that most needs to finish
//!
//! One `IrqSpinLock`, ranked beside the log ring ([`LockRank::Fbcon`]) for the same reason: it is
//! teed from the serial `write_str` with `SERIAL` held.
//!
//! - **A fault inside the console, on this CPU**, whose dump tees straight back here. [`HOLDER`]
//!   names the CPU inside, and a re-entry returns at once rather than waiting for itself.
//! - **A machine that has begun stopping**, whose other CPUs may have been halted holding the
//!   lock. Writes stop at `arch::stopping()`, and the reclaim waits a bounded time, then leaves
//!   the screen as it is.
//! - **Neither, but a long repaint on another CPU while a fault dump is being teed.** A bounded
//!   wait, then that line is dropped — from the screen, never from COM1.
//!
//! The yield and the report's three calls wait without a bound; [`yield_to_userspace`] says why.

pub mod glyphs;
pub mod text;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::smp::ArchSmp;
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::limine::Framebuffer;
use glyphs::{BLANK, GLYPH_W};
use crate::arch::timer::ArchTimer;
use text::{CELLS, Geometry, Grid, INK, MAX_COLS, PAPER};

/// A linear framebuffer of 32-bit pixels, and the console's two colours packed for it.
pub struct Screen {
    base: *mut u8,
    width: usize,
    height: usize,
    /// Bytes per pixel row, which can exceed `width * 4`.
    pitch: usize,
    ink: u32,
    paper: u32,
}

// SAFETY: `base` addresses the framebuffer, which belongs to no thread, and every access to a
// `Screen` the kernel holds goes through `CONSOLE`'s lock.
unsafe impl Send for Screen {}

impl Screen {
    /// Describe the framebuffer at `base`, whose red, green and blue bytes sit at `shifts`.
    ///
    /// `None` for a geometry the painting below could not honour: a null or misaligned base, a
    /// pitch narrower than four bytes a pixel or not a multiple of four, or a shift that does
    /// not leave a whole byte.
    ///
    /// # Safety
    ///
    /// `base` must address `pitch * height` writable bytes of 32-bit framebuffer, mapped for as
    /// long as the returned value is used and written by nothing else while the console owns it.
    pub unsafe fn new(
        base: *mut u8,
        width: usize,
        height: usize,
        pitch: usize,
        shifts: [u8; 3],
    ) -> Option<Self> {
        let min_pitch = width.checked_mul(4)?;
        if base.is_null() || (base as usize) % 4 != 0 || width == 0 || height == 0 {
            return None;
        }
        if pitch < min_pitch || pitch % 4 != 0 || shifts.iter().any(|&s| s > 24) {
            return None;
        }
        let pack = |c: [u8; 3]| {
            (c[0] as u32) << shifts[0] | (c[1] as u32) << shifts[1] | (c[2] as u32) << shifts[2]
        };
        Some(Self { base, width, height, pitch, ink: pack(INK), paper: pack(PAPER) })
    }

    fn put(&self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        // SAFETY: `x < width` and `y < height`, so the offset is below `pitch * height`, which
        // `new`'s contract makes writable. `base` and `pitch` are multiples of four (checked in
        // `new`) and so is `x * 4`, so the `u32` write is aligned.
        unsafe {
            core::ptr::write_volatile(self.base.add(y * self.pitch + x * 4) as *mut u32, color);
        }
    }

    /// The same screen, written through another mapping of the same pixels.
    ///
    /// For the measurement in `report_framebuffer_cost`: one loop, one geometry, two page-table
    /// entries, so the difference between the two timings is the mapping and nothing else.
    ///
    /// # Safety
    /// `base` must address `pitch * height` writable bytes of the *same* framebuffer, for as long
    /// as the returned value is used.
    unsafe fn with_base(&self, base: *mut u8) -> Option<Self> {
        if base.is_null() || (base as usize) % 4 != 0 {
            return None;
        }
        Some(Self { base, ..*self })
    }

    fn fill(&self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        for py in y..y_end {
            for px in x..x_end {
                self.put(px, py, color);
            }
        }
    }

    /// Draw `glyph` into cell (`row`, `col`), background and all.
    fn paint_cell(&self, geometry: Geometry, row: usize, col: usize, glyph: u8) {
        let (cell_w, cell_h, scale) = (geometry.cell_w(), geometry.cell_h(), geometry.scale);
        let (x0, y0) = (col * cell_w, row * cell_h);
        if x0 + cell_w > self.width || y0 + cell_h > self.height {
            return;
        }
        for (gy, bits) in glyphs::rows(glyph).iter().enumerate() {
            for sy in 0..scale {
                let y = y0 + gy * scale + sy;
                for gx in 0..GLYPH_W {
                    let color = if bits & (0x80 >> gx) != 0 { self.ink } else { self.paper };
                    for sx in 0..scale {
                        self.put(x0 + gx * scale + sx, y, color);
                    }
                }
            }
        }
    }
}

/// Who draws on the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// The console: every write is painted.
    Kernel,
    /// A client holds `/dev/framebuffer`: writes reach the grid and nothing is painted.
    Userspace,
    /// The hardware report holds the screen: writes reach the grid, and what is painted is the
    /// report's page.
    Report,
}

/// The text grid and, when there is one, the screen it is drawn on.
pub struct Console {
    screen: Option<Screen>,
    grid: Grid,
    /// The page on screen while [`Owner::Report`] holds it; blank otherwise.
    page: Grid,
    /// The glyph each on-screen cell shows, `MAX_COLS` to a row — what a paint compares against,
    /// so a scroll repaints the cells that changed rather than all of them.
    shown: [u8; CELLS],
    owner: Owner,
}

impl Console {
    /// A console with no screen.
    pub const fn new() -> Self {
        Self {
            screen: None,
            grid: Grid::new(),
            page: Grid::new(),
            shown: [BLANK; CELLS],
            owner: Owner::Kernel,
        }
    }

    /// Start drawing on `screen`, clearing it. `None`, and no screen, when not one cell fits.
    pub fn attach(&mut self, screen: Screen) -> Option<Geometry> {
        let geometry = Geometry::for_screen(screen.width, screen.height)?;
        self.grid.reset(geometry);
        screen.fill(0, 0, screen.width, screen.height, screen.paper);
        self.shown = [BLANK; CELLS];
        self.owner = Owner::Kernel;
        self.screen = Some(screen);
        Some(geometry)
    }

    /// Fill the whole screen, timed, through the console's own mapping and — when `also` is
    /// given — through a second mapping of the same pixels; then put the text back (Phase 5
    /// Part G's measurement).
    ///
    /// `None` when the kernel is not the one drawing: the report holds the screen for a person to
    /// read, and userspace's pixels are not ours to overwrite.
    ///
    /// **Two mappings, because the first boot's numbers said the cost is not a property of the
    /// memory.** A fill through the console's mapping ran at GiB/s on a machine whose range
    /// registers call that memory uncacheable, which is only possible if the mapping asks for
    /// something else — so what a *write* costs is a question about a page table, and the honest
    /// way to ask it is to write the same pixels twice, with the same loop, through the two
    /// mappings the system actually has.
    ///
    /// The fill paints the margins as well as the cells, so putting the text back is a repaint
    /// of every cell and nothing else.
    ///
    /// # Safety
    /// `also`, if given, must address `pitch * height` writable bytes of the same framebuffer.
    pub unsafe fn time_full_fills(&mut self, also: Option<*mut u8>) -> Option<Fills> {
        if self.owner != Owner::Kernel {
            return None;
        }
        let screen = self.screen.as_ref()?;
        let (w, h) = (screen.width, screen.height);
        // The other mapping first: whatever it paints, the console's own fill paints over.
        let other_ns = match also {
            // SAFETY: forwarded from this function's contract.
            Some(base) => unsafe { screen.with_base(base) }.map(|alt| {
                let start = crate::arch::Timer::read_ns();
                alt.fill(0, 0, w, h, alt.paper);
                crate::arch::Timer::read_ns().saturating_sub(start)
            }),
            None => None,
        };
        let start = crate::arch::Timer::read_ns();
        screen.fill(0, 0, w, h, screen.paper);
        let own_ns = crate::arch::Timer::read_ns().saturating_sub(start);
        let rows = self.grid.geometry().rows;
        if rows > 0 {
            self.paint(0, rows - 1, true);
        }
        Some(Fills { bytes: w * h * 4, own_ns, other_ns })
    }

    /// Who draws on the screen.
    pub fn owner(&self) -> Owner {
        self.owner
    }

    /// The text grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Apply a byte stream, and paint what it changed if the kernel has the screen.
    pub fn write(&mut self, bytes: &[u8]) {
        self.grid.write(bytes);
        let damage = self.grid.take_damage();
        if let (Owner::Kernel, Some((first, last))) = (self.owner, damage) {
            self.paint(first, last, false);
        }
    }

    /// Stop painting. Returns whether the kernel had the screen until now.
    pub fn yield_screen(&mut self) -> bool {
        let had = self.owner == Owner::Kernel;
        self.owner = Owner::Userspace;
        had
    }

    /// Hold the screen for the hardware report. `None`, and no change, unless the kernel has the
    /// screen now: there is no report on a console with no screen, or after userspace took it.
    pub fn hold_for_report(&mut self) -> Option<Geometry> {
        self.screen.as_ref()?;
        if self.owner != Owner::Kernel {
            return None;
        }
        self.owner = Owner::Report;
        Some(self.grid.geometry())
    }

    /// Show `page` — which must fit the rows above the last without scrolling ([`text::Pages`]
    /// cuts it so) — with `prompt` on the last row. Painted only while the report holds the
    /// screen; the cells that already show the right glyph are left alone.
    pub fn show_page(&mut self, page: &[u8], prompt: &[u8]) {
        if self.owner != Owner::Report {
            return;
        }
        let g = self.grid.geometry();
        if g.rows == 0 {
            return;
        }
        self.page.reset(g);
        self.page.write(page);
        // To the start of the last row. The page occupies the rows above it at most, so each of
        // these newlines has a row to move to and none scrolls.
        while self.page.cursor().0 + 1 < g.rows {
            self.page.write(b"\n");
        }
        if self.page.cursor().1 != 0 {
            self.page.write(b"\r");
        }
        // At most one row of it, cut between characters, so it cannot wrap off the bottom.
        self.page.write(text::Pages::new(prompt, g.cols, 1).next().unwrap_or(&[]));
        self.paint(0, g.rows - 1, false);
    }

    /// The report is over: the kernel draws again, and the screen goes back to the grid — with
    /// every line written while the report held it.
    pub fn end_report(&mut self) {
        if self.owner != Owner::Report {
            return;
        }
        self.owner = Owner::Kernel;
        self.page.reset(Geometry { scale: 1, cols: 0, rows: 0 });
        let rows = self.grid.geometry().rows;
        if rows > 0 {
            self.paint(0, rows - 1, false);
        }
    }

    /// Paint again. Taken back from userspace, that is every cell and the margins beyond them,
    /// since nothing about what is on the screen is known any more.
    pub fn reclaim(&mut self) {
        let from = core::mem::replace(&mut self.owner, Owner::Kernel);
        let g = self.grid.geometry();
        let Some(screen) = &self.screen else { return };
        if g.rows == 0 {
            return;
        }
        let everything = from == Owner::Userspace;
        if everything {
            let (text_w, text_h) = (g.cols * g.cell_w(), g.rows * g.cell_h());
            screen.fill(text_w, 0, screen.width - text_w, screen.height, screen.paper);
            screen.fill(0, text_h, text_w, screen.height - text_h, screen.paper);
        }
        self.paint(0, g.rows - 1, everything);
    }

    /// Paint rows `first..=last` from what the screen should show — the report's page while it
    /// holds the screen, the grid otherwise — skipping cells [`shown`](Self::shown) says are
    /// already right unless `everything`.
    fn paint(&mut self, first: usize, last: usize, everything: bool) {
        let Some(screen) = &self.screen else { return };
        let g = self.grid.geometry();
        let source = if self.owner == Owner::Report { &self.page } else { &self.grid };
        for row in first..=last.min(g.rows.saturating_sub(1)) {
            for col in 0..g.cols {
                let glyph = source.glyph_at(row, col);
                let at = row * MAX_COLS + col;
                if everything || self.shown[at] != glyph {
                    screen.paint_cell(g, row, col, glyph);
                    self.shown[at] = glyph;
                }
            }
        }
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

static CONSOLE: IrqSpinLock<Console> = IrqSpinLock::new(LockRank::Fbcon, Console::new());

/// One more than the index of the CPU inside [`CONSOLE`], or 0 when none is. Written only by
/// the holder, so a CPU that reads its own number here is inside already: a fault in the console
/// whose dump has teed back to it.
static HOLDER: AtomicU32 = AtomicU32::new(0);

/// Set by the first [`reclaim_for_stop`]. Nothing writes after it.
static RECLAIMED: AtomicBool = AtomicBool::new(false);

/// Spins to wait for the lock before giving up.
///
/// Ordinary writers never contend with each other — all of them hold `SERIAL` — so this is spent
/// only by a fault dump or the reclaim, waiting out a write on another CPU. It is a spin count,
/// not a time, and no repaint on the laptop has been measured against it; what it bounds is how
/// long a stuck holder can delay a CPU that is trying to stop.
const PATIENCE: u32 = 1 << 22;

/// Take the screen Limine handed over, clear it, and draw from here on.
///
/// `None` when there is nothing to draw on: a depth other than 32 bits, or a geometry
/// [`Screen::new`] refuses.
///
/// # Safety
///
/// `fb` must be Limine's live framebuffer descriptor, whose `address` stays mapped and writable
/// for the kernel's lifetime.
pub unsafe fn init(fb: &Framebuffer) -> Option<Geometry> {
    if fb.bpp != 32 {
        return None;
    }
    let shifts = [fb.red_mask_shift, fb.green_mask_shift, fb.blue_mask_shift];
    // SAFETY: forwarded from this function's contract; Limine's descriptor states the geometry
    // of the mapping at `address`, and nothing else draws on it until userspace is handed it.
    let screen = unsafe {
        Screen::new(fb.address, fb.width as usize, fb.height as usize, fb.pitch as usize, shifts)
    }?;
    let mut geometry = None;
    with_console(|c| geometry = c.attach(screen));
    geometry
}

/// Tee bytes on their way to COM1. Called with `SERIAL` held, and from the unsynchronised
/// emergency writer.
pub fn push(bytes: &[u8]) {
    if RECLAIMED.load(Ordering::Acquire) || crate::arch::stopping() {
        return;
    }
    with_console(|c| c.write(bytes));
}

/// Hand the screen to userspace: the first `/dev/framebuffer` handout calls this before the
/// handle exists. Later handouts find it done.
///
/// **Waits as long as it takes**, unlike every other entry here. A yield that gave up would
/// leave the console drawing over the desktop for the rest of the boot, and none of the hazards
/// the bounded wait exists for can reach it: it is ordinary syscall context, never inside the
/// console, and a machine that is stopping halts this CPU whether or not it waits.
pub fn yield_to_userspace() {
    // The gate reads the frame the handout would otherwise replace within milliseconds.
    #[cfg(feature = "fbcon-gate")]
    hold_for_gate();
    let me = crate::arch::Smp::current_cpu().wrapping_add(1);
    let yielded = {
        let mut console = CONSOLE.lock();
        HOLDER.store(me, Ordering::Release);
        let yielded = console.yield_screen();
        HOLDER.store(0, Ordering::Release);
        yielded
    };
    if yielded {
        // After the lock is released — this line tees back into the console, which a holder
        // would refuse — and so, like everything from here on, it is on COM1 and not on screen.
        crate::kprintln!("fbcon: /dev/framebuffer handed out; the console stops drawing");
    }
}

/// Hold the screen for the hardware report (Phase 5 Part D.3): from here until [`end_report`],
/// writes reach the grid and are not drawn, and the screen shows what [`show_page`] puts there.
///
/// `None` when there is no console on screen or the kernel no longer has it. Waits for the lock
/// without a bound, as [`yield_to_userspace`] does and for the same reasons: ordinary thread
/// context, never inside the console, before userspace.
pub fn hold_for_report() -> Option<Geometry> {
    let mut geometry = None;
    with_console_waiting(|c| geometry = c.hold_for_report());
    geometry
}

/// Show one page of the report, `prompt` on the last row. See [`Console::show_page`].
pub fn show_page(page: &[u8], prompt: &[u8]) {
    with_console_waiting(|c| c.show_page(page, prompt));
}

/// What a full-screen fill cost, through one mapping or two.
pub struct Fills {
    /// Bytes written per fill — the pixels, not the padding between rows.
    pub bytes: usize,
    /// Through the console's own mapping, the one the bootloader made.
    pub own_ns: u64,
    /// Through the second mapping, if one was given.
    pub other_ns: Option<u64>,
}

/// Time a full-screen fill through the console's mapping and optionally a second one, then
/// repaint the text — see [`Console::time_full_fills`].
///
/// # Safety
/// `also`, if given, must address the same framebuffer, writable, for the call's duration.
pub unsafe fn time_full_fills(also: Option<*mut u8>) -> Option<Fills> {
    let mut measured = None;
    // SAFETY: forwarded from this function's contract.
    with_console_waiting(|c| measured = unsafe { c.time_full_fills(also) });
    measured
}

/// End the report: the console draws again, from the grid.
pub fn end_report() {
    with_console_waiting(|c| c.end_report());
}

/// Run `f` on the console, waiting for the lock as long as it takes, with [`HOLDER`] naming this
/// CPU while it runs so a fault inside `f` does not wait for itself.
fn with_console_waiting(f: impl FnOnce(&mut Console)) {
    let me = crate::arch::Smp::current_cpu().wrapping_add(1);
    let mut console = CONSOLE.lock();
    HOLDER.store(me, Ordering::Release);
    f(&mut console);
    HOLDER.store(0, Ordering::Release);
}

/// **Under the `fbcon-gate` feature only**: keep the screen as it is for a second, so
/// `cargo xtask check-fbcon` reads this frame on every run rather than when a screendump happens
/// to land inside it.
///
/// Called with no lock held — after the timer is calibrated in `kernel_main`, and before the
/// first handout yields the screen. The handout's caller is the compositor, and nothing else
/// prints while it waits: `init` is blocked on the compositor binding `/dev/draw`.
#[cfg(feature = "fbcon-gate")]
pub fn hold_for_gate() {
    use crate::arch::timer::ArchTimer;
    const HOLD_NS: u64 = 1_000_000_000;
    let until = crate::arch::Timer::read_ns().saturating_add(HOLD_NS);
    while crate::arch::Timer::read_ns() < until {
        core::hint::spin_loop();
    }
}

/// Take the screen back for a machine that is stopping, repainted with the last rows written.
/// Once per boot; called by `stop_the_machine` after every other CPU has been told to halt.
pub fn reclaim_for_stop() {
    if RECLAIMED.swap(true, Ordering::AcqRel) {
        return;
    }
    with_console(|c| c.reclaim());
}

/// Run `f` on the console, unless this CPU is inside it already or the lock does not come free
/// within [`PATIENCE`]. Returns whether `f` ran.
///
/// **A retried `try_lock` is a wait**, which the rank tracker cannot see — it records each
/// attempt as an acquisition that did not wait, and orders none of them. That is sound here only
/// because the retry gives up: it can delay a CPU, never deadlock one. Making it unbounded would
/// need `lock()` instead (`lockrank` § Only an acquisition that waits is ordered).
fn with_console(f: impl FnOnce(&mut Console)) -> bool {
    let me = crate::arch::Smp::current_cpu().wrapping_add(1);
    if HOLDER.load(Ordering::Acquire) == me {
        return false;
    }
    for _ in 0..PATIENCE {
        if let Some(mut console) = CONSOLE.try_lock() {
            HOLDER.store(me, Ordering::Release);
            f(&mut console);
            HOLDER.store(0, Ordering::Release);
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use glyphs::GLYPH_H;

    const SHIFTS: [u8; 3] = [16, 8, 0];

    /// A host buffer standing in for a framebuffer, with `pad` extra pixels of pitch per row.
    struct Fake {
        pixels: Vec<u32>,
        width: usize,
        height: usize,
        stride: usize,
    }

    impl Fake {
        fn new(width: usize, height: usize, pad: usize) -> Self {
            let stride = width + pad;
            Self { pixels: vec![0xDEAD_BEEF; stride * height], width, height, stride }
        }

        fn screen(&mut self) -> Screen {
            // SAFETY: the buffer is `stride * height` u32s, i.e. `pitch * height` bytes, and
            // outlives every test's use of the `Screen`.
            unsafe {
                Screen::new(self.pixels.as_mut_ptr() as *mut u8, self.width, self.height, self.stride * 4, SHIFTS)
                    .unwrap()
            }
        }

        fn at(&self, x: usize, y: usize) -> u32 {
            self.pixels[y * self.stride + x]
        }

        /// Read the screen back into text the way the gate does: every cell must be exactly ink
        /// and paper, every `scale`×`scale` block one colour, and its bitmap a glyph.
        fn read(&self, g: Geometry) -> Vec<String> {
            let ink = pack(INK);
            let paper = pack(PAPER);
            (0..g.rows)
                .map(|row| {
                    let s: String = (0..g.cols)
                        .map(|col| {
                            let (x0, y0) = (col * g.cell_w(), row * g.cell_h());
                            let mut bitmap = [0u8; GLYPH_H];
                            for y in 0..g.cell_h() {
                                for x in 0..g.cell_w() {
                                    let p = self.at(x0 + x, y0 + y);
                                    assert!(p == ink || p == paper, "cell ({row},{col}) has a stray pixel {p:#x}");
                                    // A block reads as one glyph pixel only if it is one colour:
                                    // OR-ing it would pass a painter that filled a quarter of it.
                                    let block = self.at(x0 + x / g.scale * g.scale, y0 + y / g.scale * g.scale);
                                    assert_eq!(p, block, "cell ({row},{col}) has a block of two colours");
                                    if p == ink {
                                        bitmap[y / g.scale] |= 0x80 >> (x / g.scale);
                                    }
                                }
                            }
                            glyphs::identify(&bitmap).and_then(glyphs::char_of).unwrap_or('\u{FFFD}')
                        })
                        .collect();
                    s.trim_end().to_string()
                })
                .collect()
        }
    }

    fn pack(c: [u8; 3]) -> u32 {
        (c[0] as u32) << SHIFTS[0] | (c[1] as u32) << SHIFTS[1] | (c[2] as u32) << SHIFTS[2]
    }

    #[test]
    fn a_screen_it_could_not_paint_correctly_is_refused() {
        let mut buf = vec![0u32; 64 * 64];
        let base = buf.as_mut_ptr() as *mut u8;
        // SAFETY: none of these is painted; `new` only inspects the arguments.
        unsafe {
            assert!(Screen::new(base, 16, 16, 64, SHIFTS).is_some());
            assert!(Screen::new(base, 16, 16, 60, SHIFTS).is_none(), "pitch narrower than the row");
            assert!(Screen::new(base, 16, 16, 66, SHIFTS).is_none(), "pitch not whole pixels");
            assert!(Screen::new(base.add(1), 16, 16, 64, SHIFTS).is_none(), "misaligned base");
            assert!(Screen::new(core::ptr::null_mut(), 16, 16, 64, SHIFTS).is_none());
            assert!(Screen::new(base, 16, 16, 64, [16, 8, 25]).is_none(), "a shift past the top byte");
        }
    }

    #[test]
    fn what_is_written_is_what_the_screen_reads_back_including_the_padding_and_margins() {
        // 83×40 at scale 1 is 10×2 cells with a 3-pixel margin right and 8 below, and a padded
        // pitch; the padding must not be painted and the margins must be cleared.
        let mut fake = Fake::new(83, 40, 5);
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        assert_eq!(g, Geometry { scale: 1, cols: 10, rows: 2 });
        console.write("ok — go\nsecond".as_bytes());
        assert_eq!(fake.read(g), ["ok — go", "second"]);
        assert_eq!(fake.at(82, 39), pack(PAPER), "the margin was cleared");
        assert_eq!(fake.pixels[83], 0xDEAD_BEEF, "the pitch padding was not painted");
    }

    #[test]
    fn a_scroll_still_reads_back_exactly() {
        let mut fake = Fake::new(80, 64, 0);
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        assert_eq!(g.rows, 4);
        // Enough to wrap the ring several times, with lines of different lengths so a stale
        // cell from an earlier, longer line would show.
        for i in 0..23 {
            console.write(format!("{}{i}\n", "x".repeat(i % 7)).as_bytes());
        }
        let expect: Vec<String> = (0..g.rows).map(|r| {
            let s: String = (0..g.cols).map(|c| glyphs::char_of(console.grid().glyph_at(r, c)).unwrap()).collect();
            s.trim_end().to_string()
        }).collect();
        assert_eq!(fake.read(g), expect);
        // Rows 4 and a jump of 1: after the last newline the cursor sits on a cleared bottom row.
        assert_eq!(expect, ["xxxxxx20", "21", "x22", ""], "and the grid holds what was written");
    }

    /// What the `shown` comparison buys: a paint touches only the cells whose glyph changed,
    /// even inside a row it repaints. Cells that do not change are marked after they were drawn;
    /// a painter that repainted every cell of a damaged row would paint over the marks.
    #[test]
    fn a_cell_whose_glyph_did_not_change_is_not_repainted() {
        let mut fake = Fake::new(80, 64, 0);
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        let mark = 0x0012_3456;
        let last_col = (g.cols - 1) * g.cell_w();

        console.write(b"keep");
        fake.pixels[0] = mark; // cell (0, 0): `k`, in the row about to be written again
        console.write(b"ing\n");
        assert_eq!(fake.pixels[0], mark, "appending to row 0 repainted its unchanged `k`");

        // A scroll damages every row. Row 0's last cell is blank before and after, so it is
        // left alone; its first cell goes from `k` to the next line's first glyph, so it is not.
        fake.pixels[last_col] = mark;
        for _ in 0..g.rows {
            console.write(b"x\n");
        }
        assert_eq!(fake.pixels[last_col], mark, "a scroll repainted a cell that stayed blank");
        assert_ne!(fake.pixels[0], mark, "a scroll that changed the cell must repaint it");
    }

    #[test]
    fn scale_two_paints_every_glyph_pixel_as_a_block() {
        let mut fake = Fake::new(2056, 1600, 0);
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        assert_eq!(g.scale, 2);
        console.write(b"Scaled");
        assert_eq!(fake.read(g)[0], "Scaled");
    }

    #[test]
    fn a_report_page_is_what_the_screen_shows_until_the_report_ends() {
        let mut fake = Fake::new(80, 64, 0); // 10x4 cells
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        console.write(b"boot 1\nboot 2\n");
        assert_eq!(console.hold_for_report(), Some(g));
        assert_eq!(console.hold_for_report(), None, "the report cannot be held twice");
        console.show_page(b"page one\nsecond", "— 1/2 —".as_bytes());
        assert_eq!(fake.read(g), ["page one", "second", "", "— 1/2 —"]);

        // A late kernel line reaches the grid and is not drawn over the page — not even the
        // scroll it causes.
        console.write(b"late 3\nlate 4\nlate 5\n");
        assert_eq!(fake.read(g), ["page one", "second", "", "— 1/2 —"]);

        console.show_page(b"two", b"end");
        assert_eq!(fake.read(g), ["two", "", "", "end"], "nothing of the first page survives");

        console.end_report();
        assert_eq!(console.owner(), Owner::Kernel);
        let grid: Vec<String> = (0..g.rows)
            .map(|r| {
                let s: String = (0..g.cols).map(|c| glyphs::char_of(console.grid().glyph_at(r, c)).unwrap()).collect();
                s.trim_end().to_string()
            })
            .collect();
        assert_eq!(fake.read(g), grid, "the end repaints the grid, late lines and all");
        assert!(grid.iter().any(|l| l == "late 5"));
        console.write(b"after");
        assert!(fake.read(g).iter().any(|l| l == "after"), "and the kernel draws again");
    }

    #[test]
    fn there_is_no_report_on_a_screen_userspace_has() {
        let mut fake = Fake::new(80, 64, 0);
        let mut console = Box::new(Console::new());
        console.attach(fake.screen()).unwrap();
        assert!(console.yield_screen());
        assert_eq!(console.hold_for_report(), None);
        assert_eq!(console.owner(), Owner::Userspace, "a refused hold changes nothing");
        let mut bare = Box::new(Console::new());
        assert_eq!(bare.hold_for_report(), None, "nor on no screen at all");
    }

    #[test]
    fn after_the_yield_nothing_is_painted_and_the_reclaim_repaints_everything() {
        // 102×50 leaves a 6-pixel margin right and 2 below, as 1366×768 does on the laptop: the
        // reclaim has to clear what userspace drew there too.
        let mut fake = Fake::new(102, 50, 0);
        let mut console = Box::new(Console::new());
        let g = console.attach(fake.screen()).unwrap();
        console.write(b"booting\n");
        assert!(console.yield_screen());
        assert!(!console.yield_screen(), "only the first yield hands anything over");
        // Userspace draws; the kernel keeps logging.
        fake.pixels.fill(0x0012_3456);
        console.write(b"panic: here\n");
        assert!(fake.pixels.iter().all(|&p| p == 0x0012_3456), "a yielded console paints nothing");
        console.reclaim();
        assert_eq!(console.owner(), Owner::Kernel);
        assert_eq!(fake.read(g), ["booting", "panic: here", ""]);
        assert!(fake.pixels.iter().all(|&p| p == pack(INK) || p == pack(PAPER)), "no userspace pixel survives");
    }
}
