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
//! become one queued record.
//!
//! **At most one wheel per window per gesture, and the detents are *summed*** (M14 Part I,
//! PR #288 review). A wheel could not take motion's rule — `wheel` is a **delta**, so replacing
//! the old record with the new one silently loses everything but the last detent — and leaving
//! it uncoalesced would have made it the first pointer kind to take a queue slot per event, in a
//! queue whose depth is justified by there not being one. Summing is the third answer, and it is
//! the only lossless one: `3` then `2` becomes one record of `5`, which is what the person did.
//! It follows the same remove-and-append as motion, so the total arrives at the *later* record's
//! position rather than the earlier one's.
//!
//! **Only into a record that agrees about `modifiers` and `buttons`.** Those ride on every
//! pointer record and change what a scroll *means* — Ctrl-scroll is a zoom in most applications
//! — so summing across a modifier change would turn three scrolled lines and two zoomed steps
//! into five zoomed steps.
//!
//! Removing the old record and pushing the new at the **back**, rather than overwriting it
//! in place, keeps the queue in the order things happened: a motion that occurred after a
//! keystroke is delivered after it.
//!
//! **What is left uncoalesced is still not bounded by "what a human can do".** A held key
//! repeats every `REPEAT_INTERVAL_NS`, so `Outbound::Key` streams through this queue on a
//! stalled client exactly as it did before the wheel existed. That is a pre-existing property
//! of key repeat rather than something the wheel introduced, and it is the reason the sentence
//! above is about the *pointer* kinds specifically.

use alloc::vec::Vec;

use librsproto::surface::{KeyEvent, POINTER_MOTION, POINTER_WHEEL, PointerEvent};

/// One message addressed to one window.
///
/// **`Clone` rather than `Copy` since M10 Part E**: a drop carries a path and two names, which
/// are variable-length by nature — bounded at the protocol edge, but not small enough to sit
/// inline in every queued record. Thirty-two slots per session times a 512-byte path is the
/// arithmetic that decided it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outbound {
    /// A `Surface::KeyEvent`, addressed by the window **inside** the record.
    ///
    /// **No envelope copy of the window id.** These two records name their own window as of
    /// M6 C3, and the addressing the outbox does must not be able to disagree with what the
    /// client reads: an envelope that said one window while the record said another would
    /// deliver to one client a record claiming to be for a different window. The other
    /// variants keep theirs because their records are built at send time from those fields.
    Key {
        /// The record to send; `event.window` is who it goes to.
        event: KeyEvent,
    },
    /// A `Surface::PointerEvent`, addressed by the window inside the record — see [`Key`](Self::Key).
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
    /// A `Surface::Configure` a **manager** asked for, addressed to the window's client.
    ///
    /// Queued rather than sent directly, for the reason every other server-initiated record
    /// is: sent straight it competes with input on the same ring, and input is continuous. A
    /// manager's `Configure` used to go out with `SENDMODE_NOBLOCK` and its failure discarded,
    /// so a client whose ring was briefly full never resized — and the manager was told the
    /// request succeeded (PR #216 review, finding 4).
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
    ///
    /// Queued like every other server-initiated record, and for the same reason as the close
    /// request below: sent directly with `NOBLOCK` it would be lost against a client whose ring
    /// is briefly full, and a dismissal that vanishes leaves a menu on screen that the person has
    /// already walked away from.
    Dismissed {
        /// The popup a press landed outside of.
        window: u32,
    },
    /// A `Surface::CloseRequested` — somebody with the manager channel is asking this window to
    /// close.
    ///
    /// Queued like every other server-initiated record, and for the same reason: sent directly
    /// with `NOBLOCK` it would be lost against a client whose ring is briefly full — and a
    /// close request that vanishes is one the shell then has to insist on, killing a client that
    /// would have gone quietly.
    CloseRequested {
        /// Which window is being asked to close.
        window: u32,
    },
    /// A `Surface::Dropped` — a drag ended over this window (M10 Part E).
    ///
    /// Queued like every other server-initiated record. A drop that was lost to a briefly full
    /// ring would be a gesture the user completed and the system silently forgot, which is worse
    /// here than for a `Configure`: there is nothing to repeat it.
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
        /// See [`x`](Self::Dropped::x).
        y: i32,
    },
}

impl Outbound {
    /// The window this message is addressed to.
    ///
    /// **One place the addressing is decided**, which is what lets `Key` and `Pointer` carry
    /// the id only in their record: there is no second copy for it to disagree with.
    pub fn window(&self) -> u32 {
        match self {
            Outbound::Key { event } => event.window,
            Outbound::Pointer { event } => event.window,
            Outbound::Release { window, .. }
            | Outbound::Focus { window, .. }
            | Outbound::Configure { window, .. }
            | Outbound::Dropped { window, .. }
            | Outbound::Dismissed { window }
            | Outbound::CloseRequested { window } => *window,
        }
    }

    /// Whether this is pointer motion — the kind that coalesces by *replacement*.
    fn is_motion(&self) -> bool {
        matches!(self, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)
    }

    /// This record's wheel, if it is one: the window it is for, its detents, and the state it
    /// was turned under.
    ///
    /// **The state is part of the identity**, because it decides whether two turns may be added
    /// together: see the module docs on summing.
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
///
/// With motion replaced and wheel detents summed, **no pointer kind takes more than one slot
/// per window** — so this is sized against what a person can do in the time a client takes to
/// drain, not against a stream. Keys are the exception and always were: a held key repeats, so
/// `Outbound::Key` streams through here whatever the pointer is doing (PR #288 review, 3).
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
    pub fn push(&mut self, mut rec: Outbound) -> bool {
        if rec.is_motion() {
            // Remove any motion already queued for this window; the new one supersedes it.
            let w = rec.window();
            self.q.retain(|q| !(q.is_motion() && q.window() == w));
        }
        // **A wheel is added to the one already queued rather than replacing it.** A detent is a
        // delta, so the newest record is only the last part of the truth — see the module docs.
        if let Some((w, dz, mods, buttons)) = rec.as_wheel() {
            let same = |q: &Outbound| {
                matches!(q.as_wheel(), Some((qw, _, qm, qb)) if qw == w && qm == mods && qb == buttons)
            };
            if let Some(i) = self.q.iter().position(same) {
                let carried = self.q.remove(i).as_wheel().map_or(0, |(_, dz, _, _)| dz);
                if let Outbound::Pointer { event } = &mut rec {
                    // Saturating, like the compositor's own conversion into this field: a very
                    // long stall means a very long scroll, and wrapping would scroll back.
                    event.wheel = carried.saturating_add(dz);
                }
            }
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
    ///
    /// **Borrowed rather than copied out**, since a `Dropped` carries a path: the caller sends
    /// from it and then [`pop`](Self::pop)s, so a clone per send would be a clone per record for
    /// the benefit of the one variant that needs one.
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
        Outbound::Pointer {
            event: PointerEvent { window, kind: POINTER_MOTION, x, ..Default::default() },
        }
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
            event: PointerEvent { window: 1, kind: POINTER_ENTER, ..Default::default() },
        });
        o.push(Outbound::Pointer {
            event: PointerEvent { window: 1, kind: POINTER_BUTTON, ..Default::default() },
        });
        assert_eq!(o.len(), 7);
    }

    /// A wheel record for `window`, `dz` detents, turned with `mods` held.
    fn wheel(window: u32, dz: i16, mods: u16) -> Outbound {
        Outbound::Pointer {
            event: PointerEvent {
                window,
                kind: POINTER_WHEEL,
                wheel: dz,
                modifiers: mods,
                ..Default::default()
            },
        }
    }

    /// Detents are **summed** into the queued record, not replaced by it and not left to pile up.
    ///
    /// **The two obvious rules are both wrong here**, which is the whole reason this is its own
    /// case. `x` is a *position*, so motion's replace-coalescing keeps the whole truth; `wheel`
    /// is a *delta*, so replacing loses every detent but the last — a client scrolling steadily
    /// would find its page creeping. Leaving it uncoalesced instead makes it the first pointer
    /// kind to take a queue slot per event, which is precisely what `OUTBOX_MAX`'s depth is
    /// justified by not happening (PR #288 review, 3). Summing is lossless *and* bounded.
    #[test]
    fn wheel_detents_are_summed_into_one_queued_record() {
        let mut o = Outbox::new();
        for _ in 0..5 {
            assert!(!o.push(wheel(1, 1, 0)), "nothing should be discarded");
        }
        assert_eq!(o.len(), 1, "five detents are one record");
        assert_eq!(drain(&mut o), [wheel(1, 5, 0)], "and it carries all five");

        // Both directions, since a delta is signed: three down then two up is one net detent.
        let mut o = Outbox::new();
        o.push(wheel(1, 3, 0));
        o.push(wheel(1, -2, 0));
        assert_eq!(drain(&mut o), [wheel(1, 1, 0)]);
    }

    /// Summing is per window, and only across turns that mean the same thing.
    ///
    /// **`modifiers` decides what a scroll *is***: Ctrl-scroll is a zoom in most applications, so
    /// adding three scrolled lines to two zoomed steps would deliver five zoomed steps. The same
    /// argument holds for `buttons`, which is what makes a wheel turned mid-drag distinct.
    #[test]
    fn a_wheel_does_not_sum_across_a_window_or_a_modifier_change() {
        let mut o = Outbox::new();
        o.push(wheel(1, 1, 0));
        o.push(wheel(2, 1, 0));
        o.push(wheel(1, 1, MOD_CTRL));
        assert_eq!(o.len(), 3, "three different things: {:?}", o.len());
        assert_eq!(drain(&mut o), [wheel(1, 1, 0), wheel(2, 1, 0), wheel(1, 1, MOD_CTRL)]);
    }

    /// A summed wheel is delivered at the **later** record's place in the queue.
    ///
    /// The same rule motion follows, and for the same reason: a wheel turned after a keystroke
    /// must not arrive before it. Overwriting the earlier record in place would do exactly that.
    #[test]
    fn a_summed_wheel_keeps_its_place_behind_what_happened_first() {
        let mut o = Outbox::new();
        o.push(wheel(1, 1, 0));
        o.push(key(1, 30));
        o.push(wheel(1, 1, 0));
        assert_eq!(drain(&mut o), [key(1, 30), wheel(1, 2, 0)]);
    }

    /// Scrolling cannot evict a `Release`, which is the property the bound exists for.
    ///
    /// **The failure this prevents is permanent**: `libsurface`'s `Window::acquire` blocks in
    /// `sys_wait` with no timeout, so a lost `Release` hangs the client for ever with one line in
    /// a log to say so. Before the detents were summed, `OUTBOX_MAX` turns of the wheel pushed it
    /// off the front — and `nxterm` repaints its whole viewport per wheel record, so falling
    /// behind while scrolling is the expected case rather than a contrived one.
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
        // Same reasoning as `Release`: input is continuous and a focus change is not, so on
        // a shared queue the cheap message would evict the one whose loss leaves a window
        // blinking a caret it does not own.
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
        // Losing and gaining are two messages to two different windows; collapsing them
        // would leave one window believing it still has the keyboard.
        let mut o = Outbox::new();
        o.push(Outbound::Focus { window: 1, focused: false });
        o.push(Outbound::Focus { window: 2, focused: true });
        assert_eq!(o.len(), 2);
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
        assert_eq!(o.front(), Some(&key(1, 1)), "keycode 0 is the one that went");
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

/// One server→manager event, queued rather than sent.
///
/// **A separate queue from the session [`Outbox`], deliberately.** The two differ in every
/// property that shapes one: these are addressed to a *channel* rather than to a window, none of
/// them coalesces (there is no manager-side equivalent of pointer motion), and losing one is not
/// a shortened event stream but a **corrupted window list** — a manager that missed a `created`
/// has a window it will never place and never hear about again.
///
/// They are queued for the same reason session records are: sent directly with `NOBLOCK`, a
/// manager whose receive ring is briefly full silently loses the event, and the compositor has
/// no way to know. That exact defect was found in the manager's `Configure` path in review of
/// PR #216.
/// Not `Copy`: [`Title`](MgrEvent::Title) owns its string, because the queue is drained
/// later and a window can be destroyed in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MgrEvent {
    /// A registered chord was pressed — `Manage::Hotkey`.
    ///
    /// Queued like every other manager record so it arrives in order with the window events
    /// around it: a chord that moves a window and the geometry change it causes must not
    /// reach the manager the wrong way round.
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
    ///
    /// Carries the title **by value**. The queue is drained later and bounded, so a window can
    /// be destroyed between the rename and the send; holding an id and reading the title back
    /// then would report the wrong title or none.
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
    /// **The one manager event a client's own rate drives**, which is why the compositor drops a
    /// request for the state a window was last asked to be in: this queue does not coalesce and
    /// discards its oldest, so an unprivileged client in a loop would otherwise push a
    /// `Created` off the front of the manager's view of the world. The same argument `SetTitle`
    /// already makes for unchanged titles (M9 Part B).
    StateRequest(librsproto::surface::WindowState),
    /// An interactive gesture ended and asks for this rectangle — `Manage::DragEnded`.
    ///
    /// **One per gesture, at the release**, from either of the two that produce it: a resize
    /// (the rectangle the user let go at) or a move released inside a snap zone (that zone's
    /// target). The outline the user dragged never crossed this queue: reporting it per motion
    /// would put a round trip back into the one place that cannot take it, since this queue does
    /// not coalesce and evicts its oldest when full. Carries a `ConfigureEvent` because that is
    /// exactly what the manager sends back (M9 Parts E and F).
    DragEnded(librsproto::surface::ConfigureEvent),
}

/// How many manager events queue before the oldest are discarded.
///
/// Deeper than [`OUTBOX_MAX`] because the burst shape is different: a session's queue holds
/// what a *person* can do while a client drains, but a manager's holds what the *machine* can
/// do — `ui-testclient`'s churn probe creates and destroys 128 windows as fast as it can, which
/// is 256 events with no user pacing them. Sized to carry that without discarding, since a
/// discard here is a window list that has silently gone wrong.
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
    ///
    /// By reference since [`MgrEvent::Title`] owns a string; the sender only reads it, and
    /// [`pop`](Self::pop) is what removes it once it has gone out.
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
