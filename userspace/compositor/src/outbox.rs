//! What the compositor owes each client, and the order it owes it in.
//!
//! Unsolicited messages (a release, a key, a pointer record) go through a per-session queue,
//! not a bare `NOBLOCK` send. Losing a motion is cosmetic; losing a `Release` hangs the client
//! for good (`Window::acquire` waits with no timeout); and on a shared ring continuous input
//! evicts the rare message, whatever the depth.
//!
//! At most one motion per window is queued: a newer one replaces the older (the position is
//! absolute) and goes to the back, which keeps the order against keystrokes. At most one wheel
//! per window is queued and its detents are summed (a wheel is a delta), but only into a record
//! with the same `modifiers` and `buttons`, since Ctrl-scroll means something else. Keys are
//! not coalesced: a held key repeats, so it can still stream through a stalled client's queue.

use alloc::vec::Vec;

use librsproto::surface::{KeyEvent, POINTER_MOTION, POINTER_WHEEL, PointerEvent};

/// One message addressed to one window.
///
/// `Clone` rather than `Copy`: a `Dropped` owns variable-length strings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outbound {
    /// A `Surface::KeyEvent`, addressed by the window inside the record (no envelope copy).
    Key {
        /// The record to send; `event.window` is who it goes to.
        event: KeyEvent,
    },
    /// A `Surface::PointerEvent`, addressed by the window inside the record.
    Pointer {
        /// The record to send; `event.window` is who it goes to.
        event: PointerEvent,
    },
    /// A `Surface::Release` — a buffer has left the screen.
    Release {
        /// Which window it belongs to.
        window: u32,
        /// The buffer the client may draw into again.
        buffer: u32,
    },
    /// A `Surface::FocusEvent` — this window gained or lost the keyboard.
    Focus {
        /// Which window changed.
        window: u32,
        /// Whether it now has the keyboard.
        focused: bool,
    },
    /// A `Surface::Configure` a **manager** asked for; queued so a full ring cannot drop it.
    Configure {
        /// Which window is being asked to adopt the geometry.
        window: u32,
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
        /// Requested origin, x.
        x: i32,
        /// Requested origin, y.
        y: i32,
    },
    /// A `Surface::Dismissed` — a press landed outside this popup.
    Dismissed {
        /// The popup a press landed outside of.
        window: u32,
    },
    /// A `Surface::CloseRequested` — somebody with the manager channel asks this window to close.
    CloseRequested {
        /// Which window is being asked to close.
        window: u32,
    },
    /// A `Surface::WindowState` — the state the manager put this window in.
    WindowState {
        /// Which window.
        window: u32,
        /// One of the `WINDOW_STATE_*` values.
        state: u32,
    },
    /// A `Surface::Dropped` — a drag ended over this window; nothing could repeat a lost one.
    Dropped {
        /// Which window it landed on.
        window: u32,
        /// The acceptor it matched, by the name that window declared.
        acceptor: alloc::string::String,
        /// What the payload is.
        kind: u32,
        /// The payload.
        path: alloc::string::String,
        /// What to call it on screen.
        name: alloc::string::String,
        /// Where the pointer was, window-local.
        x: i32,
        /// Where the pointer was, window-local y.
        y: i32,
    },
}

impl Outbound {
    /// The window this message is addressed to.
    pub fn window(&self) -> u32 {
        match self {
            Outbound::Key { event } => event.window,
            Outbound::Pointer { event } => event.window,
            Outbound::Release { window, .. }
            | Outbound::Focus { window, .. }
            | Outbound::Configure { window, .. }
            | Outbound::Dropped { window, .. }
            | Outbound::Dismissed { window }
            | Outbound::CloseRequested { window }
            | Outbound::WindowState { window, .. } => *window,
        }
    }

    fn is_motion(&self) -> bool {
        matches!(self, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)
    }

    /// This record's wheel as (window, detents, modifiers, buttons); the state is part of identity.
    fn as_wheel(&self) -> Option<(u32, i16, u16, u16)> {
        match self {
            Outbound::Pointer { event } if event.kind == POINTER_WHEEL => {
                Some((event.window, event.wheel, event.modifiers, event.buttons))
            }
            _ => None,
        }
    }
}

/// How many messages a session queues before the oldest are discarded.
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
    /// Returns `true` if something was discarded; the caller logs it.
    pub fn push(&mut self, mut rec: Outbound) -> bool {
        if rec.is_motion() {
            let w = rec.window();
            self.q.retain(|q| !(q.is_motion() && q.window() == w));
        }
        // A wheel is a delta, so it is summed into the queued one, not replaced.
        if let Some((w, dz, mods, buttons)) = rec.as_wheel() {
            let same =
                |q: &Outbound| matches!(q.as_wheel(), Some((qw, _, qm, qb)) if qw == w && qm == mods && qb == buttons);
            if let Some(i) = self.q.iter().position(same) {
                let carried = self.q.remove(i).as_wheel().map_or(0, |(_, dz, _, _)| dz);
                if let Outbound::Pointer { event } = &mut rec {
                    // Saturating: wrapping would scroll back.
                    event.wheel = carried.saturating_add(dz);
                }
            }
        }
        let mut discarded = false;
        if self.q.len() >= OUTBOX_MAX {
            // Oldest goes: the newest describes the world as it is now.
            self.q.remove(0);
            self.dropped = self.dropped.saturating_add(1);
            discarded = true;
        }
        self.q.push(rec);
        discarded
    }

    /// The oldest queued message, if any.
    pub fn front(&self) -> Option<&Outbound> {
        self.q.first()
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
    use librsproto::surface::{MOD_CTRL, POINTER_BUTTON, POINTER_ENTER};

    fn motion(window: u32, x: i32) -> Outbound {
        Outbound::Pointer { event: PointerEvent { window, kind: POINTER_MOTION, x, ..Default::default() } }
    }

    fn key(window: u32, keycode: u16) -> Outbound {
        Outbound::Key { event: KeyEvent::new(window, keycode, 1, 0) }
    }

    fn drain(o: &mut Outbox) -> Vec<Outbound> {
        let mut out = Vec::new();
        while let Some(r) = o.front().cloned() {
            out.push(r);
            o.pop();
        }
        out
    }

    #[test]
    fn a_drag_of_a_hundred_motions_queues_one_record() {
        let mut o = Outbox::new();
        for x in 0..100 {
            assert!(!o.push(motion(1, x)), "nothing should be discarded");
        }
        assert_eq!(o.len(), 1);
        assert_eq!(drain(&mut o), [motion(1, 99)], "the newest position survives");
    }

    #[test]
    fn a_motion_that_happened_after_a_key_is_delivered_after_it() {
        // Coalescing must not reorder: the newest motion belongs after the key.
        let mut o = Outbox::new();
        o.push(motion(1, 10));
        o.push(key(1, 30));
        o.push(motion(1, 20));
        assert_eq!(drain(&mut o), [key(1, 30), motion(1, 20)]);
    }

    #[test]
    fn coalescing_is_per_window() {
        let mut o = Outbox::new();
        o.push(motion(1, 10));
        o.push(motion(2, 20));
        o.push(motion(1, 11));
        assert_eq!(drain(&mut o), [motion(2, 20), motion(1, 11)]);
    }

    #[test]
    fn nothing_but_motion_coalesces() {
        let mut o = Outbox::new();
        for k in 0..5 {
            o.push(key(1, k));
        }
        o.push(Outbound::Pointer { event: PointerEvent { window: 1, kind: POINTER_ENTER, ..Default::default() } });
        o.push(Outbound::Pointer { event: PointerEvent { window: 1, kind: POINTER_BUTTON, ..Default::default() } });
        assert_eq!(o.len(), 7);
    }

    fn wheel(window: u32, dz: i16, mods: u16) -> Outbound {
        Outbound::Pointer {
            event: PointerEvent { window, kind: POINTER_WHEEL, wheel: dz, modifiers: mods, ..Default::default() },
        }
    }

    #[test]
    fn wheel_detents_are_summed_into_one_queued_record() {
        let mut o = Outbox::new();
        for _ in 0..5 {
            assert!(!o.push(wheel(1, 1, 0)), "nothing should be discarded");
        }
        assert_eq!(o.len(), 1, "five detents are one record");
        assert_eq!(drain(&mut o), [wheel(1, 5, 0)], "and it carries all five");

        let mut o = Outbox::new();
        o.push(wheel(1, 3, 0));
        o.push(wheel(1, -2, 0));
        assert_eq!(drain(&mut o), [wheel(1, 1, 0)]);
    }

    #[test]
    fn a_wheel_does_not_sum_across_a_window_or_a_modifier_change() {
        let mut o = Outbox::new();
        o.push(wheel(1, 1, 0));
        o.push(wheel(2, 1, 0));
        o.push(wheel(1, 1, MOD_CTRL));
        assert_eq!(o.len(), 3, "three different things: {:?}", o.len());
        assert_eq!(drain(&mut o), [wheel(1, 1, 0), wheel(2, 1, 0), wheel(1, 1, MOD_CTRL)]);
    }

    #[test]
    fn a_summed_wheel_keeps_its_place_behind_what_happened_first() {
        let mut o = Outbox::new();
        o.push(wheel(1, 1, 0));
        o.push(key(1, 30));
        o.push(wheel(1, 1, 0));
        assert_eq!(drain(&mut o), [key(1, 30), wheel(1, 2, 0)]);
    }

    /// A long scroll cannot evict a `Release`, whose loss hangs the client.
    #[test]
    fn a_release_survives_a_long_scroll() {
        let mut o = Outbox::new();
        o.push(Outbound::Release { window: 1, buffer: 7 });
        for _ in 0..OUTBOX_MAX * 4 {
            assert!(!o.push(wheel(1, 1, 0)), "a scroll discarded a queued record");
        }
        assert_eq!(o.len(), 2, "the release and one summed wheel");
        assert_eq!(o.front(), Some(&Outbound::Release { window: 1, buffer: 7 }));
    }

    #[test]
    fn a_focus_change_is_never_coalesced_away_by_input() {
        let mut o = Outbox::new();
        o.push(Outbound::Focus { window: 1, focused: true });
        for x in 0..100 {
            o.push(motion(1, x));
        }
        assert_eq!(o.len(), 2);
        assert_eq!(o.front(), Some(&Outbound::Focus { window: 1, focused: true }));
    }

    #[test]
    fn both_halves_of_a_focus_change_survive() {
        let mut o = Outbox::new();
        o.push(Outbound::Focus { window: 1, focused: false });
        o.push(Outbound::Focus { window: 2, focused: true });
        assert_eq!(o.len(), 2);
    }

    #[test]
    fn a_release_is_never_coalesced_away_by_input() {
        // A lost `Release` hangs `acquire` for good.
        let mut o = Outbox::new();
        o.push(Outbound::Release { window: 1, buffer: 7 });
        for x in 0..100 {
            o.push(motion(1, x));
        }
        assert_eq!(o.len(), 2);
        assert_eq!(drain(&mut o), [Outbound::Release { window: 1, buffer: 7 }, motion(1, 99)], "and it is still first");
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
        assert_eq!(o.front(), Some(&key(1, 1)), "keycode 0 is the one that went");
    }

    #[test]
    fn a_coalesced_motion_does_not_count_against_the_bound() {
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

/// One server→manager event, queued rather than sent.
///
/// Separate from [`Outbox`]: addressed to a channel, never coalesced, and losing one corrupts
/// the manager's window list. `Title` owns its string as the window may be gone by send time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MgrEvent {
    /// A registered chord was pressed — `Manage::Hotkey`; queued to keep order with window events.
    Hotkey(librsproto::surface::MgrHotkey),
    /// A window was created: id, role and the geometry its client asked for.
    Created(librsproto::surface::MgrWindowCreated),
    /// A window went away.
    Destroyed {
        /// Which window.
        window: u32,
    },
    /// A window's position or size changed, for any reason.
    Geometry(librsproto::surface::ConfigureEvent),
    /// A window was renamed by its client.
    Title {
        /// Which window.
        window: u32,
        /// Its new title, already truncated to `MAX_TITLE`.
        title: alloc::string::String,
    },
    /// The keyboard moved to or from a window.
    Focus {
        /// Which window.
        window: u32,
        /// Whether it now has the keyboard.
        focused: bool,
    },
    /// The work area is not what it was last announced to be.
    LayoutChanged(librsproto::surface::MgrLayout),
    /// A client asked to be minimised, maximised or restored.
    ///
    /// A client's own rate drives this, so a request for the state the window was last asked to
    /// be in is dropped; else a looping client could push a `Created` off the full queue.
    StateRequest(librsproto::surface::WindowState),
    /// An interactive gesture ended, asking for a rectangle — `Manage::DragEnded`.
    ///
    /// One per gesture, at the release; never per motion, as this queue does not coalesce.
    DragEnded(librsproto::surface::MgrDragEnded),
}

/// How many manager events queue before the oldest are discarded.
///
/// Deeper than [`OUTBOX_MAX`]: the machine, not a person, paces it (the churn probe makes 256).
pub const MGR_OUTBOX_MAX: usize = 512;

/// The manager's pending events, oldest first.
#[derive(Default)]
pub struct MgrOutbox {
    q: Vec<MgrEvent>,
    dropped: u32,
}

impl MgrOutbox {
    /// An empty queue.
    pub fn new() -> Self {
        Self { q: Vec::new(), dropped: 0 }
    }

    /// Queue an event, discarding the oldest if full. `true` if something was discarded.
    pub fn push(&mut self, ev: MgrEvent) -> bool {
        let mut discarded = false;
        if self.q.len() >= MGR_OUTBOX_MAX {
            self.q.remove(0);
            self.dropped = self.dropped.saturating_add(1);
            discarded = true;
        }
        self.q.push(ev);
        discarded
    }

    /// The oldest queued event, if any.
    pub fn front(&self) -> Option<&MgrEvent> {
        self.q.first()
    }

    /// Discard the oldest — call after it has been sent.
    pub fn pop(&mut self) {
        if !self.q.is_empty() {
            self.q.remove(0);
        }
    }

    /// Whether anything is queued.
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    /// How many events have been discarded.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Forget everything queued — the manager went away.
    pub fn clear(&mut self) {
        self.q.clear();
        self.dropped = 0;
    }
}
