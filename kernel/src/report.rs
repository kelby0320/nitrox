//! The hardware report: the kernel log, held on the screen a page at a time before userspace
//! starts, on a boot whose command line asks for it (Phase 5 Part D.3).
//!
//! Every boot logs what it found (Part D.1), and on a machine with a serial port that is enough.
//! The laptop Phase 5 targets has none, and its screen holds 48 rows that scroll as userspace
//! starts and are gone when the compositor takes the display a second later. A report boot stops
//! here instead — after the drivers have bound, every CPU has come up and the framebuffer is
//! recorded, before `init` — and shows the log from its first line, holding each page until a key
//! is pressed. Then the same boot carries on, so it also shows whether userspace comes up.
//!
//! What is shown is the log itself, read back out of [`klog`](crate::klog): there is no second
//! statement of any fact to drift from the first.
//!
//! ## What ends a page, and what ends the report
//!
//! - **A key press** turns the page, counted by the i8042 driver as it decodes. Which key is never
//!   looked at. The keys that turned pages are discarded before userspace starts, so none reaches
//!   the first program to read the keyboard.
//! - **No key within the bound** ends the whole report, not the page: a keyboard that does not
//!   work would otherwise cost the bound once per page, minutes on a long log, which looks exactly
//!   like the hang a report exists to diagnose. The bound is `hwreport=<seconds>`.
//! - **No keyboard at all** — no i8042 answered — and nothing is held: the report says so and the
//!   boot carries on at once.
//! - **No console on the screen**, and there is nothing to hold either. COM1 already has every
//!   line.

use crate::arch::cpu::ArchCpu;
use crate::arch::timer::ArchTimer;
use crate::drivers::ps2;
use crate::fbcon;
use crate::libkern::KVec;

/// Show the kernel log so far a page at a time, each page waiting up to `page_wait_secs` for a
/// key. Boot-time, from the boot thread, with interrupts enabled (the key and the clock both
/// arrive by interrupt) and before any userspace exists.
pub fn run(page_wait_secs: u32) {
    if !ps2::keyboard_present() {
        crate::kprintln!("report: no keyboard answered, so nothing can turn a page — not holding");
        return;
    }

    // The log first, then the screen: what is shown is the log as it stood when the report began.
    // A line printed after this reaches COM1 and the console's grid, and is on the screen again
    // when the report ends.
    let mut log: KVec<u8> = KVec::new();
    let size = crate::klog::len();
    if log.try_reserve(size).is_err() {
        crate::kprintln!("report: no memory for a {} byte copy of the log — not holding", size);
        return;
    }
    while log.push_within_capacity(0).is_ok() {}
    let copied = crate::klog::copy_into(&mut log);
    let log = &log[..copied];

    let Some(geometry) = fbcon::hold_for_report() else {
        crate::kprintln!("report: no console on the screen to hold — COM1 has every line");
        return;
    };
    // The last row is the prompt's.
    let rows = geometry.rows.saturating_sub(1);
    let pages = fbcon::text::Pages::new(log, geometry.cols, rows).count();
    crate::kprintln!(
        "report: {} page(s) of {} bytes, each held up to {} s for a key",
        pages,
        log.len(),
        page_wait_secs
    );

    let wait_ns = page_wait_secs as u64 * 1_000_000_000;
    let mut shown = 0;
    for (index, page) in fbcon::text::Pages::new(log, geometry.cols, rows).enumerate() {
        let mut prompt = PromptBuf::new();
        let _ = core::fmt::Write::write_fmt(
            &mut prompt,
            format_args!("— page {}/{} — any key —", index + 1, pages),
        );
        fbcon::show_page(page, prompt.as_bytes());
        shown += 1;
        if !wait_for_key(wait_ns) {
            crate::kprintln!(
                "report: no key within {} s on page {}/{} — ending the report",
                page_wait_secs,
                index + 1,
                pages
            );
            break;
        }
    }

    fbcon::end_report();
    ps2::drain_keyboard();
    crate::kprintln!(
        "report: done, {} of {} page(s) shown; the keys that turned them were discarded",
        shown,
        pages
    );
}

/// Wait until a key is pressed or `wait_ns` passes. Returns whether a key was pressed.
///
/// Halts between looks rather than spinning: every CPU's periodic tick wakes it within a tick,
/// and so does the keyboard's own interrupt when it lands on this CPU.
fn wait_for_key(wait_ns: u64) -> bool {
    let before = ps2::key_presses();
    let deadline = crate::arch::Timer::read_ns().saturating_add(wait_ns);
    loop {
        if ps2::key_presses() != before {
            return true;
        }
        if crate::arch::Timer::read_ns() >= deadline {
            return false;
        }
        // SAFETY: ring 0, in a boot thread with the scheduler running; `idle_halt` enables
        // interrupts as it halts, so the tick or the key that ends this wait can always wake it.
        unsafe { crate::arch::Cpu::idle_halt() };
    }
}

/// A prompt line, formatted without allocating. Longer than any screen's row is fine: the console
/// shows one row of it.
struct PromptBuf {
    bytes: [u8; 64],
    len: usize,
}

impl PromptBuf {
    const fn new() -> Self {
        Self { bytes: [0; 64], len: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for PromptBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}
