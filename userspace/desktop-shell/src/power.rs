//! Logging out, restarting and shutting down: the ending dialog and the broker's answer.

use super::*;

/// Where a dialog of `size` goes over `base`, kept inside the work area.
///
/// Centred on its parent, then clamped, so a dialog wider than its parent keeps its buttons
/// reachable; one that will not fit sits at the work area's origin, losing its bottom-right.
/// `base` is `(x, y, w, h)`: the parent's rectangle, or the work area with no known parent.
pub(super) fn centre_dialog(base: (i32, i32, u32, u32), size: (u32, u32), work: &MgrLayout) -> (i32, i32) {
    let x = base.0.saturating_add((base.2 as i32 - size.0 as i32) / 2);
    let y = base.1.saturating_add((base.3 as i32 - size.1 as i32) / 2);
    // `.max(0)` keeps the clamp's bounds ordered; `clamp` panics on an inverted range.
    let max_x = work.work_x.saturating_add((work.work_w as i32 - size.0 as i32).max(0));
    let max_y = work.work_y.saturating_add((work.work_h as i32 - size.1 as i32).max(0));
    (x.clamp(work.work_x, max_x), y.clamp(work.work_y, max_y))
}

/// Where the waiting dialog hangs: right-aligned under the power button, as the power menu is.
pub(super) fn ending_anchor(clock: &str, theme: &Theme, font: &Font, screen: Screen) -> (i32, i32) {
    let view = panel::top_bar(clock, None, None, theme);
    let l = layout(&view, Rect::new(0, 0, screen.width, BAR_H), &FontMetrics::new(font, theme.font_px));
    libui::layout::locate(&view, &l, panel::POWER_KEY).map_or(
        (screen.width as i32 - libui::widget::DIALOG_W as i32 - panel::MENU_INSET_X, BAR_H as i32 + panel::MENU_DROP),
        |word| panel::menu_anchor_right(word, libui::widget::DIALOG_W),
    )
}

pub(super) fn ending_view(c: &Closing, hovered: Option<u64>, theme: &Theme) -> Element<EndingMsg> {
    let ending = match c.asked() {
        Asked::Person(e) => e,
        Asked::Stop => Ending::LogOut,
    };
    panel::ending_dialog(ending, &c.question(), hovered, theme)
}

/// Bring the waiting dialog up, or draw it again with what it names now.
///
/// A popup of the top bar's, not a `dialog`: a dialog is held for the manager to place, and the
/// manager is this shell, which would wait on itself. Its origin is logged; `check-logout` aims at
/// its buttons from it.
pub(super) fn show_ending_dialog(
    session: &mut Session<ChannelTransport>,
    win: &mut Option<Child>,
    c: &Closing,
    parent: u32,
    at: (i32, i32),
    theme: &Theme,
    font: &Font,
) {
    if let Some(d) = win.as_mut() {
        let view = ending_view(c, d.hovered_key(), theme);
        d.present(session, &view, font, theme);
        Line::new().s(b"desktop-shell: ").s(c.question().0.as_bytes()).end();
        return;
    }
    let view = ending_view(c, None, theme);
    let Some(mut d) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: waiting dialog CreateWindow FAILED\n");
        return;
    };
    if !d.present(session, &view, font, theme) {
        d.close(session);
        return;
    }
    Line::new()
        .s(b"desktop-shell: waiting dialog open, window ")
        .u(d.id() as u64)
        .s(b" at ")
        .i(at.0 as i64)
        .s(b",")
        .i(at.1 as i64)
        .s(b": ")
        .s(c.question().0.as_bytes())
        .end();
    *win = Some(d);
}

/// The session ends, every window closed or ended anyway: the shell exits, and
/// `desktop-session-mgr` tells the broker and presents the greeter. Restart and Shut down ask the
/// broker instead ([`ask_to_power`]).
pub(super) fn end_session(asked: Asked) -> ! {
    if asked == Asked::Stop {
        stop();
    }
    kprint(b"desktop-shell: logging out, exiting\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// How long the shell waits for the view broker before calling it gone; it answers at once.
const BROKER_WAIT_NS: u64 = 5_000_000_000;

fn broker_deadline() -> u64 {
    now_ns().map_or(u64::MAX, |n| n.saturating_add(BROKER_WAIT_NS))
}

/// Whether this session's policy would refuse a Restart or a Shut down, asked of the broker with
/// `Decide` before any window is asked to close. `None` to go on; the request after the windows
/// close still decides.
pub(super) fn power_refusal(session_ns: u64) -> Option<Refusal> {
    let ch = libviews::broker(session_ns);
    if ch == 0 {
        return Some(Refusal::Failed(alloc::string::String::from(failed::NO_BROKER)));
    }
    let answer = libviews::decide(ch, POWER_VIEW, POWER_PROGRAM, broker_deadline());
    libprompt::ipc::close(ch);
    match answer {
        Ok((outcome, why)) => Refusal::from_answer(outcome, why),
        Err(f) => Some(Refusal::Failed(alloc::string::String::from(f.why()))),
    }
}

/// Ask the view broker for `shutdown` in the `power` view (`--reboot` for Restart), as `with power
/// shutdown` does. The broker's channel while `shutdown` runs, or why it is not running.
///
/// An application's namespace goes to the broker, not this shell's: the session's binds `/dev/draw`
/// whole and reaches `manage`; the builder's one reaches `/dev/draw/new` alone.
///
/// No password is asked for: the desktop has no prompt, so the channel is closed, the broker's cue
/// to drop the request, and the refusal says so.
pub(super) fn ask_to_power(l: &Launcher<'_>, ending: Ending) -> Result<u64, Refusal> {
    use libviews::Outcome;
    let failure = |why: &str| Refusal::Failed(alloc::string::String::from(why));
    let ch = libviews::broker(l.session_ns);
    if ch == 0 {
        return Err(failure(failed::NO_BROKER));
    }
    let Some(built) = build_app_namespace(l) else {
        libprompt::ipc::close(ch);
        return Err(failure(failed::NO_NAMESPACE));
    };
    // Verified as a launch's is: `shutdown`'s view not reaching `manage` rests on the same bind.
    let (home, desktop, devices, storage, user) =
        (!l.home.is_empty(), l.desktop != 0, l.devices != 0, l.storage != 0, !l.user.is_empty());
    if !verify_app_namespace(built.ns, home, desktop, devices, storage, user) {
        libprompt::ipc::close(built.ns);
        libprompt::ipc::close(ch);
        kprint(b"desktop-shell: application namespace is not gated; not asking the broker\n");
        return Err(failure(failed::NO_NAMESPACE));
    }
    let args = ending.power_args().unwrap_or(&[]);
    let handed = libviews::Handed { ns: built.ns, ..libviews::Handed::default() };
    let refusal = match libviews::request(ch, POWER_VIEW, POWER_PROGRAM, args, &[], handed, broker_deadline()) {
        Ok((Outcome::Started, _)) => {
            Line::new()
                .s(b"desktop-shell: ")
                .s(ending.title().as_bytes())
                .s(b": the broker started ")
                .s(POWER_PROGRAM.as_bytes())
                .s(b" in the ")
                .s(POWER_VIEW.as_bytes())
                .s(b" view")
                .end();
            return Ok(ch);
        }
        Ok((Outcome::NeedPassword, _)) => Refusal::Password,
        Ok((Outcome::Denied { .. }, why)) => Refusal::Refused(why),
        Err(f) => failure(f.why()),
    };
    libprompt::ipc::close(ch);
    Err(refusal)
}

/// Say why a Restart or a Shut down did not happen, on the console and in a dialog under the power
/// button, replacing any refusal up. Its origin is logged.
#[allow(clippy::too_many_arguments, reason = "the refusal, where it goes and how to draw it")]
pub(super) fn show_refusal(
    session: &mut Session<ChannelTransport>,
    slot: &mut Option<(Child, Ending, Refusal)>,
    ending: Ending,
    refusal: Refusal,
    parent: u32,
    at: (i32, i32),
    theme: &Theme,
    font: &Font,
) {
    // Escaped: a broker's reason can quote the policy's own text back.
    Line::new().s(b"desktop-shell: ").s(ending.title().as_bytes()).s(b" ").untrusted(refusal.log().as_bytes()).end();
    if let Some((d, _, _)) = slot.take() {
        d.close(session);
    }
    let view = panel::refusal_dialog(ending, &refusal, None, theme);
    let Some(mut d) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: refusal dialog CreateWindow FAILED\n");
        return;
    };
    if !d.present(session, &view, font, theme) {
        d.close(session);
        return;
    }
    Line::new()
        .s(b"desktop-shell: refusal dialog open, window ")
        .u(d.id() as u64)
        .s(b" at ")
        .i(at.0 as i64)
        .s(b",")
        .i(at.1 as i64)
        .end();
    *slot = Some((d, ending, refusal));
}

/// Open the desktop-name prompt: above the bottom bar's right-hand end, where the desktop's name
/// is — or below the top bar's, on a session with no bottom bar.
pub(super) fn open_prompt(
    session: &mut Session<ChannelTransport>,
    top: u32,
    bottom: Option<u32>,
    name: &TextFieldState,
    theme: &Theme,
    font: &Font,
    screen: Screen,
) -> Option<Child> {
    let view = panel::name_prompt(name, theme);
    let h = libui::layout::measure(
        &view,
        libui::layout::Constraints::loose(libdraw::geom::Size::new(u32::MAX / 4, u32::MAX / 4)),
        &FontMetrics::new(font, theme.font_px),
    )
    .h as i32;
    let x = screen.width as i32 - panel::PROMPT_W as i32 - panel::MENU_INSET_X;
    // A popup's offset is from its parent's origin, so above the bottom bar is a negative one.
    let (parent, at) = match bottom {
        Some(b) => (b, (x, -(h + panel::MENU_DROP))),
        None => (top, (x, BAR_H as i32 + panel::MENU_DROP)),
    };
    let Some(mut c) = Child::open(session, Role::Popup { parent }, at, &view, font, theme, BUFFERS) else {
        kprint(b"desktop-shell: name prompt CreateWindow FAILED\n");
        return None;
    };
    if !c.present(session, &view, font, theme) {
        c.close(session);
        return None;
    }
    Line::new().s(b"desktop-shell: name prompt open, window ").u(c.id() as u64).end();
    Some(c)
}

pub(super) fn close_prompt(
    session: &mut Session<ChannelTransport>,
    prompt: &mut Option<Child>,
    name: &mut TextFieldState,
) {
    if let Some(c) = prompt.take() {
        let id = c.id();
        c.close(session);
        name.clear();
        Line::new().s(b"desktop-shell: name prompt closed, window ").u(id as u64).end();
    }
}
