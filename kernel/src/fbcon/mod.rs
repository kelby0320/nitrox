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
//! The yield alone waits without a bound; [`yield_to_userspace`] says why.

pub mod glyphs;
pub mod text;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::smp::ArchSmp;
use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::limine::Framebuffer;
use glyphs::{BLANK, GLYPH_W};
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
}

/// The text grid and, when there is one, the screen it is drawn on.
pub struct Console {
    screen: Option<Screen>,
    grid: Grid,
    /// The glyph each on-screen cell shows, `MAX_COLS` to a row — what a paint compares against,
    /// so a scroll repaints the cells that changed rather than all of them.
    shown: [u8; CELLS],
    owner: Owner,
}

impl Console {
    /// A console with no screen.
    pub const fn new() -> Self {
        Self { screen: None, grid: Grid::new(), shown: [BLANK; CELLS], owner: Owner::Kernel }
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

    fn paint(&mut self, first: usize, last: usize, everything: bool) {
        let Some(screen) = &self.screen else { return };
        let g = self.grid.geometry();
        for row in first..=last.min(g.rows.saturating_sub(1)) {
            for col in 0..g.cols {
                let glyph = self.grid.glyph_at(row, col);
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
        /// and paper, and its bitmap must be a glyph.
        fn read(&self, g: Geometry) -> Vec<String> {
            let ink = pack(INK);
            let paper = pack(PAPER);
            (0..g.rows)
                .map(|row| {
                    let s: String = (0..g.cols)
                        .map(|col| {
                            let mut bitmap = [0u8; GLYPH_H];
                            for y in 0..g.cell_h() {
                                for x in 0..g.cell_w() {
                                    let p = self.at(col * g.cell_w() + x, row * g.cell_h() + y);
                                    assert!(p == ink || p == paper, "cell ({row},{col}) has a stray pixel {p:#x}");
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
    fn a_scroll_repaints_only_what_changed_and_still_reads_back_exactly() {
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
    fn after_the_yield_nothing_is_painted_and_the_reclaim_repaints_everything() {
        let mut fake = Fake::new(96, 48, 0);
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
