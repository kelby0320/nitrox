//! What the compositor owes each client, and the order it owes it in.
//!
//! Every message the compositor sends unsolicited — a buffer release, a key, a pointer
//! record — goes through a per-session queue here rather than straight down a
//! `SENDMODE_NOBLOCK` send. The reason is that the two kinds have wildly different
//! consequences when they are lost, and until now they shared one four-message ring:
//!
//! - Losing a **motion** is cosmetic. The next one supersedes it.
//! - Losing a **`Release`** hangs the client *permanently*: `libsurface`'s `Window::acquire`
//!   blocks in `sys_wait` with no timeout, so the buffer stays busy and nothing ever wakes
//!   it. The only trace is one line in the compositor's log.
//!
//! Input is continuous and a `Release` is not, so on a shared ring the cheap message
//! reliably evicts the expensive one. That is not a depth problem: no depth is "enough"
//! against a stream, it only moves the threshold — and a rarer permanent hang is *worse* to
//! diagnose than a reproducible one.
//!
//! ## Coalescing is what makes the bound real
//!
//! **At most one motion per window is ever queued.** A newer motion removes the older one
//! and takes its place at the back. This is what X11 and Wayland both do, and it works for
//! the same reason there: a motion record carries an absolute window-local position, so the
//! newest one says everything the older ones did. A hundred motion events during a drag
//! become one queued record, and the discrete events — keys, buttons, crossings, releases —
//! are bounded by what a human can physically do.
//!
//! Removing the old motion and pushing the new at the **back**, rather than overwriting it
//! in place, keeps the queue in the order things happened: a motion that occurred after a
//! keystroke is delivered after it.

use alloc::vec::Vec;

use librsproto::surface::{KeyEvent, POINTER_MOTION, PointerEvent};

/// One message addressed to one window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outbound {
    /// A `Surface::KeyEvent` for `window`.
    Key {
        /// Which window it is addressed to.
        window: u32,
        /// The record to send.
        event: KeyEvent,
    },
    /// A `Surface::PointerEvent` for `window`.
    Pointer {
        /// Which window it is addressed to.
        window: u32,
        /// The record to send.
        event: PointerEvent,
    },
    /// A `Surface::Release` — a buffer has left the screen.
    Release {
        /// Which window it belongs to.
        window: u32,
        /// The buffer the client may draw into again.
        buffer: u32,
    },
}

impl Outbound {
    /// The window this message is addressed to.
    pub fn window(&self) -> u32 {
        match self {
            Outbound::Key { window, .. }
            | Outbound::Pointer { window, .. }
            | Outbound::Release { window, .. } => *window,
        }
    }

    /// Whether this is pointer motion — the only kind that coalesces.
    fn is_motion(&self) -> bool {
        matches!(self, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)
    }
}

/// How many messages a session queues before the oldest are discarded.
///
/// With motion coalesced this holds *discrete* events only — keys, buttons, crossings,
/// releases — so it is sized against what a person can do in the time a client takes to
/// drain, not against a stream.
pub const OUTBOX_MAX: usize = 32;

/// One session's pending messages, oldest first.
#[derive(Default)]
pub struct Outbox {
    q: Vec<Outbound>,
    dropped: u32,
}

impl Outbox {
    /// An empty outbox.
    pub fn new() -> Self {
        Self { q: Vec::new(), dropped: 0 }
    }

    /// Queue a message, coalescing motion and discarding the oldest if full.
    ///
    /// Returns `true` if something had to be discarded — the caller logs that, because a
    /// silently shortened event stream is the failure this whole module exists to make
    /// visible rather than merely rarer.
    pub fn push(&mut self, rec: Outbound) -> bool {
        if rec.is_motion() {
            // Remove any motion already queued for this window; the new one supersedes it.
            let w = rec.window();
            self.q.retain(|q| !(q.is_motion() && q.window() == w));
        }
        let mut discarded = false;
        if self.q.len() >= OUTBOX_MAX {
            // Oldest, for the same reason `libsurface`'s queue does: the newest describes the
            // world as it is now, and a client that has fallen this far behind is better
            // served by the present than by the past.
            self.q.remove(0);
            self.dropped = self.dropped.saturating_add(1);
            discarded = true;
        }
        self.q.push(rec);
        discarded
    }

    /// The oldest queued message, if any.
    pub fn front(&self) -> Option<Outbound> {
        self.q.first().copied()
    }

    /// Discard the oldest queued message — call after it has been sent.
    pub fn pop(&mut self) {
        if !self.q.is_empty() {
            self.q.remove(0);
        }
    }

    /// How many messages are queued.
    pub fn len(&self) -> usize {
        self.q.len()
    }

    /// Whether anything is queued.
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// How many messages have been discarded on this session.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Forget everything queued — the session is gone.
    pub fn clear(&mut self) {
        self.q.clear();
        self.dropped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{POINTER_BUTTON, POINTER_ENTER};

    fn motion(window: u32, x: i32) -> Outbound {
        Outbound::Pointer {
            window,
            event: PointerEvent { kind: POINTER_MOTION, x, ..Default::default() },
        }
    }

    fn key(window: u32, keycode: u16) -> Outbound {
        Outbound::Key {
            window,
            event: KeyEvent { keycode, pressed: 1, modifiers: 0, _pad: 0 },
        }
    }

    fn drain(o: &mut Outbox) -> Vec<Outbound> {
        let mut out = Vec::new();
        while let Some(r) = o.front() {
            out.push(r);
            o.pop();
        }
        out
    }

    #[test]
    fn a_drag_of_a_hundred_motions_queues_one_record() {
        // The whole point. Without this, one cursor movement fills any ring you care to
        // name and evicts whatever was behind it — which is how the D2 gate lost a
        // keystroke behind twelve motion events.
        let mut o = Outbox::new();
        for x in 0..100 {
            assert!(!o.push(motion(1, x)), "nothing should be discarded");
        }
        assert_eq!(o.len(), 1);
        assert_eq!(drain(&mut o), [motion(1, 99)], "the newest position survives");
    }

    #[test]
    fn a_motion_that_happened_after_a_key_is_delivered_after_it() {
        // Coalescing must not reorder. Overwriting the old motion *in place* would deliver
        // the newest position at the oldest motion's slot — before a keystroke that
        // actually came first.
        let mut o = Outbox::new();
        o.push(motion(1, 10));
        o.push(key(1, 30));
        o.push(motion(1, 20));
        assert_eq!(drain(&mut o), [key(1, 30), motion(1, 20)]);
    }

    #[test]
    fn coalescing_is_per_window() {
        // Two windows' cursors are two different facts. Collapsing across them would report
        // one window's position to the other.
        let mut o = Outbox::new();
        o.push(motion(1, 10));
        o.push(motion(2, 20));
        o.push(motion(1, 11));
        assert_eq!(drain(&mut o), [motion(2, 20), motion(1, 11)]);
    }

    #[test]
    fn nothing_but_motion_coalesces() {
        // Every key press is its own fact; collapsing them loses typing. Crossings and
        // buttons likewise — an enter followed by an enter is not one enter.
        let mut o = Outbox::new();
        for k in 0..5 {
            o.push(key(1, k));
        }
        o.push(Outbound::Pointer {
            window: 1,
            event: PointerEvent { kind: POINTER_ENTER, ..Default::default() },
        });
        o.push(Outbound::Pointer {
            window: 1,
            event: PointerEvent { kind: POINTER_BUTTON, ..Default::default() },
        });
        assert_eq!(o.len(), 7);
    }

    #[test]
    fn a_release_is_never_coalesced_away_by_input() {
        // The message whose loss is unrecoverable: `libsurface`'s `acquire` blocks forever on a
        // buffer that is never released. A hundred motions must not cost it.
        let mut o = Outbox::new();
        o.push(Outbound::Release { window: 1, buffer: 7 });
        for x in 0..100 {
            o.push(motion(1, x));
        }
        assert_eq!(o.len(), 2);
        assert_eq!(
            drain(&mut o),
            [Outbound::Release { window: 1, buffer: 7 }, motion(1, 99)],
            "and it is still first"
        );
    }

    #[test]
    fn overflow_discards_the_oldest_and_reports_it() {
        let mut o = Outbox::new();
        for k in 0..(OUTBOX_MAX as u16) {
            assert!(!o.push(key(1, k)));
        }
        assert!(o.push(key(1, 999)), "the one that overflowed says so");
        assert_eq!(o.len(), OUTBOX_MAX);
        assert_eq!(o.dropped(), 1);
        assert_eq!(o.front(), Some(key(1, 1)), "keycode 0 is the one that went");
    }

    #[test]
    fn a_coalesced_motion_does_not_count_against_the_bound() {
        // Filling with discrete events and then dragging must not push them out: the drag
        // occupies one slot however long it lasts.
        let mut o = Outbox::new();
        for k in 0..(OUTBOX_MAX as u16 - 1) {
            o.push(key(1, k));
        }
        for x in 0..1000 {
            assert!(!o.push(motion(1, x)), "no discard at x={x}");
        }
        assert_eq!(o.len(), OUTBOX_MAX);
        assert_eq!(o.dropped(), 0);
    }

    #[test]
    fn clearing_forgets_the_loss_count_too() {
        // A slot is reused by a *different* client, which is not owed the previous one's
        // history — the same reasoning as the per-session rejection budget.
        let mut o = Outbox::new();
        for k in 0..(OUTBOX_MAX as u16 + 2) {
            o.push(key(1, k));
        }
        assert!(o.dropped() > 0);
        o.clear();
        assert!(o.is_empty());
        assert_eq!(o.dropped(), 0);
    }
}
