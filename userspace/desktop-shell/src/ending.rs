//! **Ending a session** (administration Part F.2): the windows asked to close, the wait for them,
//! and what the waiting dialog names while any is left — pure, so it is host-tested.
//!
//! **Windows, not processes.** The shell closes each application's process handle at launch, since
//! it is not their supervisor, and as the manager it is told of every window whoever opened it. So
//! a window is what it asks: `Manage::RequestClose`, which its client may answer with a question —
//! the editor's "discard unsaved changes?" — and `Manage::Close` only when the person says End
//! anyway. **Normal windows only**: a dialog is its parent's to answer for, and `nxedit` reads a
//! manager's close on its question as *keep editing*, so asking a dialog would take the question
//! away (PR #345 review).
//!
//! The binary owns the windows and the requests; this owns the decisions: what is still open, when
//! the dialog comes up, what it says, and when the session may end.
//!
//! **Restart and Shut down** (Part F.3) end the same way, and then ask the view broker for
//! `shutdown` in the `power` view, as `with power shutdown` does: [`Ending::power_args`] is what
//! they ask for, and a [`Refusal`] what the dialog says when it does not happen.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// What ending the session is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ending {
    /// The shell exits once its windows are gone, and the greeter comes back.
    LogOut,
    /// **The machine restarts** (Part F.3): once the windows are gone the shell asks the view
    /// broker for `shutdown --reboot` in the [`POWER_VIEW`], and `service-mgr`'s sequence then ends
    /// every session, this one included.
    Restart,
    /// **The machine stops**: `shutdown` in the [`POWER_VIEW`], the same way.
    ShutDown,
}

/// The view Restart and Shut down ask for: the seeded policy lets anyone run `shutdown` in it, with
/// no password.
pub const POWER_VIEW: &str = "power";

/// The program they ask to run there.
pub const POWER_PROGRAM: &str = "shutdown";

impl Ending {
    /// What the power menu's row, the dialogs' titles and the log call it.
    pub fn title(self) -> &'static str {
        match self {
            Ending::LogOut => "Log out",
            Ending::Restart => "Restart",
            Ending::ShutDown => "Shut down",
        }
    }

    /// **What it asks the view broker to run once the windows are gone**: [`POWER_PROGRAM`]'s
    /// arguments, or `None` for Log out, which asks nobody.
    pub fn power_args(self) -> Option<&'static [&'static str]> {
        match self {
            Ending::LogOut => None,
            Ending::Restart => Some(&["--reboot"]),
            Ending::ShutDown => Some(&[]),
        }
    }
}

/// **Why a Restart or a Shut down did not happen** (Part F.3), which a dialog says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// **The policy asks for the person's password, and the desktop has no prompt to ask with.**
    /// Refused rather than asked for in some other way: the graphical prompt's design says what one
    /// must guarantee (`docs/design/graphical-prompt.md`), and this is its trigger firing.
    Password,
    /// The policy said no, or the broker did: its reason.
    Refused(String),
    /// Nothing said no, and it did not happen: why.
    Failed(String),
}

/// **Why a Restart or a Shut down did not happen when nothing refused it**, as the dialog says —
/// here, where the tests that check they fit it can see them.
pub mod failed {
    /// The session's namespace has no `/dev/views`.
    pub const NO_BROKER: &str = "this session has no view broker";
    /// The application-shaped namespace the request hands the broker could not be built.
    pub const NO_NAMESPACE: &str = "no namespace could be built to ask from";
    /// The broker's channel closed while `shutdown` ran.
    pub const BROKER_GONE: &str = "the view broker went away";
}

impl Refusal {
    /// **`shutdown` ran and did not start a shutdown**: it exited with `code`, or `crashed`. Its
    /// own reason is on the console; the desktop hands it no stream to say it on.
    pub fn exited(code: i32, crashed: bool) -> Refusal {
        Refusal::Failed(if crashed {
            String::from("shutdown crashed")
        } else {
            format!("shutdown stopped with status {code}")
        })
    }

    /// What the broker's answer to `Decide` means for a Restart or a Shut down: `None` to go on.
    pub fn from_answer(outcome: libviews::Outcome, why: String) -> Option<Refusal> {
        match outcome {
            libviews::Outcome::Started => None,
            libviews::Outcome::NeedPassword => Some(Refusal::Password),
            libviews::Outcome::Denied { .. } => Some(Refusal::Refused(why)),
        }
    }

    /// **The dialog's two lines**: that `ending` did not happen, and why — in `libui`'s fixed
    /// question, like the waiting dialog's. A password's says where one can be asked for.
    pub fn lines(&self, ending: Ending) -> (String, String) {
        match self {
            Refusal::Password => (
                String::from("The policy asks for your password, which"),
                String::from("only a terminal can ask for yet."),
            ),
            Refusal::Refused(why) => (format!("{} was refused:", ending.title()), why.clone()),
            Refusal::Failed(why) => (format!("{} did not happen:", ending.title()), why.clone()),
        }
    }

    /// What the console says after the title: whether it was refused, and why — a password's
    /// naming the prompt whose trigger it is.
    pub fn log(&self) -> String {
        match self {
            Refusal::Password => String::from(
                "refused: the policy asks for a password, and the desktop has no graphical prompt yet \
                 (the trigger in docs/design/graphical-prompt.md)",
            ),
            Refusal::Refused(why) => format!("refused: {why}"),
            Refusal::Failed(why) => format!("did not happen: {why}"),
        }
    }
}

/// Who asked for the session to end, which decides how long it waits and whether it asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// A person, from the power menu. The dialog comes up after [`DIALOG_AFTER_NS`], and nothing
    /// ends until every window has gone or the person says End anyway.
    Person(Ending),
    /// **A shutdown started elsewhere** — a terminal, the serial console — reaching the shell as a
    /// terminate request. No dialog, and the shell goes after [`STOP_WAIT_NS`] whatever is left.
    Stop,
}

/// **How long the windows have before the dialog names what is left**: half a second, so a quick
/// close is never interrupted by a dialog that is gone again before it can be read.
pub const DIALOG_AFTER_NS: u64 = 500_000_000;

/// **How long a shutdown started elsewhere waits for the windows**: 3 s, **inside the 5 s
/// `libsession::spawn_leader` gives a leader**, with room. That clock starts when the supervisor
/// sends the request and the shell learns of it later, so a wait as long as the supervisor's would
/// always end after the supervisor had given up, and every window that declined would make the
/// leader one "still running after it was asked to stop" (PR #345 review).
pub const STOP_WAIT_NS: u64 = 3_000_000_000;

/// The most windows the dialog names by title; the rest are counted. **Two**, since the dialog is
/// `libui`'s fixed question, with two lines: the count, and the names.
pub const MAX_NAMED: usize = 2;

/// **A session end under way.**
pub struct Closing {
    asked: Asked,
    /// Each window asked to close and still open, and its title as the dialog names it.
    open: Vec<(u32, String)>,
    /// **The windows ended anyway**, which the compositor destroys when it gets to it: listed until
    /// then, and neither asked again nor waited for.
    ended: Vec<u32>,
    /// When it began, on the monotonic clock.
    began: u64,
    /// Whether the dialog is up.
    dialog: bool,
    /// Whether what the dialog says has changed since it was last drawn.
    changed: bool,
}

/// What the shell does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    /// Nothing, until `until` on the monotonic clock or until something happens: `u64::MAX` for no
    /// deadline.
    Wait {
        /// When to ask again.
        until: u64,
    },
    /// Bring the dialog up, or draw it again: what it names has changed.
    Dialog,
    /// **End the session**: every window has gone, the person said End anyway, or a stop's time
    /// is up.
    End(Asked),
}

impl Closing {
    /// An end asked for by `asked`, beginning at `now`. Nothing is open until the first
    /// [`sync`](Self::sync), which returns every window to ask.
    pub fn begin(asked: Asked, now: u64) -> Closing {
        Closing { asked, open: Vec::new(), ended: Vec::new(), began: now, dialog: false, changed: false }
    }

    /// Who asked.
    pub fn asked(&self) -> Asked {
        self.asked
    }

    /// **Bring the end up to date with the windows open now**, as `(id, title)`: forget the ones
    /// that have gone, take a changed title, and return the ones not yet asked — every one the
    /// first time, and after that any window opened while the end waits — which the caller asks to
    /// close. A session's windows are all asked, whenever they appeared.
    pub fn sync(&mut self, windows: &[(u32, String)]) -> Vec<u32> {
        let before = self.open.len();
        self.open.retain(|(id, _)| windows.iter().any(|(w, _)| w == id));
        self.ended.retain(|id| windows.iter().any(|(w, _)| w == id));
        let mut changed = self.open.len() != before;
        let mut fresh = Vec::new();
        for (id, title) in windows.iter().filter(|(id, _)| !self.ended.contains(id)) {
            match self.open.iter_mut().find(|(o, _)| o == id) {
                Some((_, t)) if t != title => {
                    *t = title.clone();
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.open.push((*id, title.clone()));
                    fresh.push(*id);
                    changed = true;
                }
            }
        }
        self.changed |= changed && self.dialog;
        fresh
    }

    /// **What to do next, at `now`.**
    pub fn next(&mut self, now: u64) -> Next {
        if self.open.is_empty() {
            return Next::End(self.asked);
        }
        match self.asked {
            Asked::Stop => {
                let until = self.began.saturating_add(STOP_WAIT_NS);
                if now >= until { Next::End(self.asked) } else { Next::Wait { until } }
            }
            Asked::Person(_) => {
                let at = self.began.saturating_add(DIALOG_AFTER_NS);
                if !self.dialog && now >= at || self.changed {
                    self.dialog = true;
                    self.changed = false;
                    Next::Dialog
                } else if self.dialog {
                    Next::Wait { until: u64::MAX }
                } else {
                    Next::Wait { until: at }
                }
            }
        }
    }

    /// Whether the dialog is up.
    pub fn dialog(&self) -> bool {
        self.dialog
    }

    /// **What the dialog says, in its two lines**: how many windows are still open, and up to
    /// [`MAX_NAMED`] of their titles with how many more there are after them.
    pub fn question(&self) -> (String, String) {
        let n = self.open.len();
        let count = if n == 1 {
            String::from("Waiting for 1 window to close:")
        } else {
            format!("Waiting for {n} windows to close:")
        };
        let titles: Vec<&str> = self.open.iter().take(MAX_NAMED).map(|(_, t)| t.as_str()).collect();
        let mut names = titles.join(", ");
        if n > MAX_NAMED {
            names.push_str(&format!(" and {} more", n - MAX_NAMED));
        }
        (count, names)
    }

    /// **End anyway**: the windows still open, which the caller destroys (`Manage::Close`), and
    /// nothing left to wait for — the next [`next`](Self::next) is [`Next::End`].
    ///
    /// **Remembered, since a destroy is not immediate**: the windows are still listed on the next
    /// [`sync`](Self::sync), and one taken for a window opened while the end waited would be asked
    /// again and named by the dialog, which is what `check-logout`'s first End anyway found
    /// (administration Part F.3).
    pub fn end_anyway(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self.open.iter().map(|(id, _)| *id).collect();
        self.open.clear();
        self.ended.extend_from_slice(&ids);
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn windows(list: &[(u32, &str)]) -> Vec<(u32, String)> {
        list.iter().map(|&(id, t)| (id, String::from(t))).collect()
    }

    const T0: u64 = 1_000_000_000;

    /// **Every window is asked once**, the first time, and a window opened while the end waits is
    /// asked too; one that closes is forgotten, and with none left the session ends.
    #[test]
    fn every_window_is_asked_once_and_the_last_to_close_ends_it() {
        let mut c = Closing::begin(Asked::Person(Ending::LogOut), T0);
        assert_eq!(c.sync(&windows(&[(7, "nxterm"), (9, "notes.txt")])), vec![7, 9]);
        assert_eq!(c.sync(&windows(&[(7, "nxterm"), (9, "notes.txt")])), Vec::<u32>::new());
        assert_eq!(c.sync(&windows(&[(7, "nxterm"), (9, "notes.txt"), (11, "later")])), vec![11]);
        assert_eq!(c.next(T0), Next::Wait { until: T0 + DIALOG_AFTER_NS });
        c.sync(&windows(&[(9, "notes.txt")]));
        assert_eq!(c.next(T0 + 1), Next::Wait { until: T0 + DIALOG_AFTER_NS });
        c.sync(&windows(&[]));
        assert_eq!(c.next(T0 + 2), Next::End(Asked::Person(Ending::LogOut)));
    }

    /// **The dialog comes up at half a second, not before**, and is drawn again only when what it
    /// names changes — a window closing, or one retitled.
    #[test]
    fn the_dialog_comes_up_at_half_a_second_and_follows_the_windows() {
        let mut c = Closing::begin(Asked::Person(Ending::LogOut), T0);
        c.sync(&windows(&[(7, "nxterm"), (9, "notes.txt")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS - 1), Next::Wait { until: T0 + DIALOG_AFTER_NS });
        assert!(!c.dialog());
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS), Next::Dialog);
        assert!(c.dialog());
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 1), Next::Wait { until: u64::MAX });
        assert_eq!(c.question(), (String::from("Waiting for 2 windows to close:"), String::from("nxterm, notes.txt")));
        c.sync(&windows(&[(9, "notes.txt")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 2), Next::Dialog);
        assert_eq!(c.question(), (String::from("Waiting for 1 window to close:"), String::from("notes.txt")));
        c.sync(&windows(&[(9, "notes.txt *")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 3), Next::Dialog);
        c.sync(&windows(&[(9, "notes.txt *")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 4), Next::Wait { until: u64::MAX });
    }

    /// **A quick close never sees the dialog**: every window gone inside the half second ends the
    /// session with the dialog never up.
    #[test]
    fn a_quick_close_never_shows_the_dialog() {
        let mut c = Closing::begin(Asked::Person(Ending::LogOut), T0);
        c.sync(&windows(&[(7, "nxterm")]));
        c.sync(&windows(&[]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 5), Next::End(Asked::Person(Ending::LogOut)));
        assert!(!c.dialog());
        // And a session with no windows ends at once.
        let mut none = Closing::begin(Asked::Person(Ending::LogOut), T0);
        assert!(none.sync(&[]).is_empty());
        assert_eq!(none.next(T0), Next::End(Asked::Person(Ending::LogOut)));
    }

    /// **End anyway** hands back what is still open, to destroy, and the session ends.
    #[test]
    fn end_anyway_names_what_is_left_and_ends() {
        let mut c = Closing::begin(Asked::Person(Ending::LogOut), T0);
        c.sync(&windows(&[(7, "nxterm"), (9, "notes.txt")]));
        c.sync(&windows(&[(9, "notes.txt")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS), Next::Dialog);
        assert_eq!(c.end_anyway(), vec![9]);
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 1), Next::End(Asked::Person(Ending::LogOut)));
        // **Still listed until the compositor has destroyed it**, and neither asked again nor
        // waited for — while a window opened since is.
        assert_eq!(c.sync(&windows(&[(9, "notes.txt")])), Vec::<u32>::new());
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS + 2), Next::End(Asked::Person(Ending::LogOut)));
        assert_eq!(c.sync(&windows(&[(9, "notes.txt"), (12, "later")])), vec![12]);
        assert_eq!(c.question().1, "later");
    }

    /// **A stop waits 3 s, and never asks**: no dialog however long it waits, and the end at the
    /// bound whatever is still open — the bound tested at its neighbour.
    #[test]
    fn a_stop_waits_three_seconds_without_a_dialog() {
        let mut c = Closing::begin(Asked::Stop, T0);
        c.sync(&windows(&[(9, "notes.txt")]));
        assert_eq!(c.next(T0 + DIALOG_AFTER_NS), Next::Wait { until: T0 + STOP_WAIT_NS });
        assert_eq!(c.next(T0 + STOP_WAIT_NS - 1), Next::Wait { until: T0 + STOP_WAIT_NS });
        assert!(!c.dialog());
        assert_eq!(c.next(T0 + STOP_WAIT_NS), Next::End(Asked::Stop));
        // **Inside the leader's bound, with a second to spare**, which is the whole reason for the
        // number: the supervisor's clock starts before the shell hears of the stop.
        assert!(STOP_WAIT_NS + 1_000_000_000 <= libsession::LEADER_STOP_NS);
    }

    /// **Restart and Shut down ask for `shutdown`, and Log out asks nobody**; Restart's is the
    /// reboot. A row that asked for nothing would end the session and leave the machine running.
    #[test]
    fn restart_and_shut_down_ask_for_shutdown_and_log_out_asks_nobody() {
        assert_eq!(Ending::LogOut.power_args(), None);
        assert_eq!(Ending::Restart.power_args(), Some(&["--reboot"][..]));
        assert_eq!(Ending::ShutDown.power_args(), Some(&[][..]));
        assert_eq!((POWER_VIEW, POWER_PROGRAM), ("power", "shutdown"));
    }

    /// **What the broker's answer means**: one that would start goes on; a password, or a refusal,
    /// is said before a window is asked to close — each in its own words.
    #[test]
    fn an_answer_that_would_not_start_it_is_a_refusal() {
        use libviews::Outcome;
        assert_eq!(Refusal::from_answer(Outcome::Started, String::new()), None);
        assert_eq!(Refusal::from_answer(Outcome::NeedPassword, String::new()), Some(Refusal::Password));
        let why = String::from("no rule lets alice use `power`");
        let denied = Outcome::Denied { retry: false };
        assert_eq!(Refusal::from_answer(denied, why.clone()), Some(Refusal::Refused(why.clone())));
        assert_eq!(
            Refusal::Refused(why.clone()).lines(Ending::ShutDown),
            (String::from("Shut down was refused:"), why)
        );
        assert_eq!(Refusal::Failed(String::from("x")).lines(Ending::Restart).0, "Restart did not happen:");
        // **The password's refusal names the prompt's trigger** on the console, where the design
        // doc's reader will look for it.
        assert!(Refusal::Password.log().contains("docs/design/graphical-prompt.md"));
    }

    /// **The dialog names two and counts the rest**, at the bound and past it.
    #[test]
    fn the_dialog_names_two_and_counts_the_rest() {
        let mut c = Closing::begin(Asked::Person(Ending::LogOut), T0);
        c.sync(&windows(&[(1, "a"), (2, "b")]));
        assert_eq!(c.question(), (String::from("Waiting for 2 windows to close:"), String::from("a, b")));
        c.sync(&windows(&[(1, "a"), (2, "b"), (3, "c")]));
        assert_eq!(c.question().1, "a, b and 1 more");
        c.sync(&windows(&[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")]));
        assert_eq!(c.question(), (String::from("Waiting for 5 windows to close:"), String::from("a, b and 3 more")));
    }
}
