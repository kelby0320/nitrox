//! What reaches the shell outside a call it made: a stage's exit, a request to stop, and the
//! terminal's `Ctrl-C`.

use alloc::vec::Vec;
use core::cell::Cell;
use libkern::abi::KIND_TERMINATE_REQUESTED;
use libos::{Handle, Notify, Only, Ref, log};

const KIND_CHILD_EXITED: u32 = 0x0200;
/// `ExitStatus.kind` for a normal exit; anything else means the process died.
const EXIT_KIND_NORMAL: u32 = 0;

/// The shell's notification queue and the two flags the rest of it reads.
pub(crate) struct Signals {
    notify: Option<Handle<Notify, Only>>,
    /// Set when the terminal has reported `Ctrl-C` (§11h). The interrupt can arrive in any exchange,
    /// so every path receiving from the tty channel records it here; the caller decides what it
    /// means.
    interrupted: Cell<bool>,
    /// This shell has been asked to exit: a `TerminateRequested` from its session's supervisor, at a
    /// shutdown. Unlike `interrupted` it is sticky, so no `catch` recovers from it; the REPL exits
    /// once what was running has unwound.
    stop_asked: Cell<bool>,
}

impl Signals {
    pub(crate) fn new(notify: Option<Handle<Notify, Only>>) -> Self {
        Signals { notify, interrupted: Cell::new(false), stop_asked: Cell::new(false) }
    }

    pub(crate) fn notify(&self) -> Option<Ref<'_, Notify, Only>> {
        self.notify.as_ref().map(|n| n.as_ref())
    }

    pub(crate) fn stop_asked(&self) -> bool {
        self.stop_asked.get()
    }

    pub(crate) fn interrupted(&self) -> bool {
        self.interrupted.get()
    }

    pub(crate) fn interrupt(&self) {
        self.interrupted.set(true);
    }

    /// Whether a `Ctrl-C` was recorded, which is forgotten.
    pub(crate) fn take_interrupt(&self) -> bool {
        self.interrupted.replace(false)
    }

    /// Read every notification queued while a pipeline runs: a `ChildExited` is kept in `exits` as
    /// `(code, crashed)`, and a `TerminateRequested` marks the shell to exit. Answers whether one
    /// came, for the caller to pass on to its stages. Shared by the capture wait and the reap, so an
    /// exit read by the first is counted by the second.
    pub(crate) fn drain(&self, exits: &mut Vec<(i32, bool)>) -> bool {
        let Some(notify) = &self.notify else { return false };
        let mut requested = false;
        // WouldBlock ends it: drained.
        while let Ok(Some(n)) = notify.recv() {
            if n.kind == KIND_TERMINATE_REQUESTED {
                self.stop_asked.set(true);
                requested = true;
            } else if n.kind == KIND_CHILD_EXITED {
                let exit_kind = u32::from_le_bytes([n.body[4], n.body[5], n.body[6], n.body[7]]);
                let code = i32::from_le_bytes([n.body[8], n.body[9], n.body[10], n.body[11]]);
                exits.push((code, exit_kind != EXIT_KIND_NORMAL));
            }
        }
        requested
    }
}

/// Say this shell was asked to stop, and exit.
pub(crate) fn stop() -> ! {
    log::write(b"nxsh: asked to stop, exiting\n");
    libos::exit(crate::EXIT_OK as i32)
}
