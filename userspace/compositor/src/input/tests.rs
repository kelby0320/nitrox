use super::*;
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, DROP_KIND_DIR, DROP_KIND_FILE, Edge, MOD_SHIFT, Role,
    SURFACE_FORMAT_XRGB8888,
};

const SCREEN: Rect = Rect::new(0, 0, 640, 480);
const BTN_LEFT: u16 = 0x110;

/// A window at `(x, y)` of `w × h`, committed and configured: routing ignores any other.
fn win(stack: &mut WindowStack, role: Role, x: i32, y: i32, w: u32, h: u32) -> u32 {
    let id = stack.create(&CreateWindowRequest::new(w, h, role)).expect("create");
    stack.mark_configured(id);
    let _ = stack.place(id, Point::new(x, y)).expect("origin");
    stack
        .attach(&AttachBufferRequest {
            window: id,
            buffer: 1,
            width: w,
            height: h,
            pitch: w * 4,
            format: SURFACE_FORMAT_XRGB8888,
        })
        .expect("attach");
    stack
        .commit(&CommitRequest { window: id, buffer: 1, damage_x: 0, damage_y: 0, damage_w: w, damage_h: h })
        .expect("commit");
    id
}

fn go(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Vec<Outbound> {
    let mut out = Vec::new();
    r.route(&ev, s, &mut out);
    out
}

fn warp(r: &mut InputRouter, s: &mut WindowStack, x: i32, y: i32) {
    let p = r.pointer();
    go(r, s, motion(x - p.x, y - p.y));
}

/// The interrupt time every helper stamps; its milliseconds are neither zero nor round.
const T: u64 = 7_123_456_789;
const T_MS: u64 = T / 1_000_000;

fn motion(dx: i32, dy: i32) -> Logical {
    Logical::Motion { dx, dy, buttons: 0, modifiers: 0, time_ns: T }
}

fn drag(dx: i32, dy: i32) -> Logical {
    Logical::Motion { dx, dy, buttons: 1, modifiers: 0, time_ns: T }
}

fn key(keycode: u16, pressed: bool) -> Logical {
    Logical::Key { keycode, pressed, modifiers: 0, locks: librsproto::surface::LOCK_NUM, time_ns: T }
}

/// A key transition with modifiers held, Num Lock on as from boot: a chord ignores locks.
fn chord(keycode: u16, pressed: bool, modifiers: u16) -> Logical {
    Logical::Key { keycode, pressed, modifiers, locks: librsproto::surface::LOCK_NUM, time_ns: T }
}

const MOD_SUPER: u16 = librsproto::surface::MOD_META;
const KEY_2: u16 = 3;

fn two_windows(s: &mut WindowStack) -> (u32, u32) {
    let src = win(s, Role::Normal, 0, 0, 200, 200);
    let dst = win(s, Role::Normal, 400, 0, 200, 200);
    (src, dst)
}

fn begin_drag(r: &mut InputRouter, s: &mut WindowStack, src: u32, kind: u32, to: (i32, i32)) -> Option<Outline> {
    warp(r, s, 20, 20);
    go(r, s, button(true));
    let started = r.start_drag(src, kind, "/home/a.txt", "a.txt", s).expect("the grab is held");
    let p = r.pointer();
    go(r, s, drag(to.0 - p.x, to.1 - p.y));
    started
}

fn drop_of(out: &[Outbound]) -> Option<&Outbound> {
    out.iter().find(|o| matches!(o, Outbound::Dropped { .. }))
}

#[test]
fn a_drag_highlights_a_window_that_takes_it_and_delivers_on_release() {
    let mut s = WindowStack::new();
    let (src, dst) = two_windows(&mut s);
    s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
    let mut r = InputRouter::new(SCREEN);

    begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
    assert_eq!(r.outline(), Some(Rect::new(400, 0, 200, 200)), "the window that would take it is outlined");

    let out = go(&mut r, &mut s, button(false));
    let Some(Outbound::Dropped { window, acceptor, kind, path, name, x, y }) = drop_of(&out) else {
        panic!("no drop was delivered: {out:?}");
    };
    assert_eq!(*window, dst);
    assert_eq!(
        (acceptor.as_str(), kind, path.as_str(), name.as_str()),
        ("document", &DROP_KIND_FILE, "/home/a.txt", "a.txt")
    );
    assert_eq!((*x, *y), (50, 60), "the pointer, relative to the window it landed on");
    assert_eq!(r.outline(), None, "and the highlight goes with the gesture");
}

#[test]
fn a_window_that_does_not_take_this_kind_is_neither_highlighted_nor_dropped_on() {
    let mut s = WindowStack::new();
    let (src, dst) = two_windows(&mut s);
    s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
    let mut r = InputRouter::new(SCREEN);

    begin_drag(&mut r, &mut s, src, DROP_KIND_DIR, (450, 60));
    assert_eq!(r.outline(), None, "a folder over a files-only window highlights nothing");

    let out = go(&mut r, &mut s, button(false));
    assert!(drop_of(&out).is_none(), "and letting go delivers nothing");
}

#[test]
fn a_window_with_no_acceptor_is_neither_highlighted_nor_dropped_on() {
    // A window that declared nothing is as inert as one that declared the wrong kind.
    let mut s = WindowStack::new();
    let (src, dst) = two_windows(&mut s);
    let _ = dst;
    let mut r = InputRouter::new(SCREEN);

    begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
    assert_eq!(r.outline(), None);

    let out = go(&mut r, &mut s, button(false));
    assert!(drop_of(&out).is_none());
}

#[test]
fn a_drag_does_not_highlight_the_window_it_came_out_of() {
    let mut s = WindowStack::new();
    let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
    s.declare_acceptor(src, "self", DROP_KIND_FILE).unwrap();
    let mut r = InputRouter::new(SCREEN);

    let started = begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (60, 60));
    assert!(started.is_none(), "nothing to highlight when the drag begins");
    assert_eq!(r.outline(), None);
    let out = go(&mut r, &mut s, button(false));
    assert!(drop_of(&out).is_none(), "and dropping a thing back where it came from is a no-op");
}

#[test]
fn a_drag_that_ends_any_way_but_the_button_delivers_nothing() {
    // Two ways a gesture ends without the button: an input loss, and the grab's window leaving.
    for lost_input in [true, false] {
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
        let mut r = InputRouter::new(SCREEN);

        begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
        assert!(r.outline().is_some(), "precondition: the target is highlighted");

        let out = if lost_input {
            go(&mut r, &mut s, Logical::Dropped { time_ns: T })
        } else {
            s.set_minimized(src, true).expect("minimize");
            go(&mut r, &mut s, drag(1, 0))
        };
        assert!(drop_of(&out).is_none(), "an interrupted gesture is not somebody letting go");
        assert_eq!(r.outline(), None, "and the highlight comes down with the gesture");
    }
}

#[test]
fn the_finished_guard_is_what_refuses_a_drop_and_not_only_its_callers() {
    // Called directly: only a direct call can see the `finished` guard.
    let mut s = WindowStack::new();
    let (src, dst) = two_windows(&mut s);
    s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
    let mut r = InputRouter::new(SCREEN);
    begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
    assert_eq!(r.over, Some(dst), "precondition: a drop would land on the target");

    let (event, gone) = r.stop_carrying(&s, false);
    assert!(event.is_none(), "an unfinished gesture delivers nothing");
    assert!(gone.is_some(), "and its highlight still comes down");
    assert!(r.carrying.is_none(), "the payload goes either way");
}

#[test]
fn a_drag_is_refused_unless_the_pointer_is_holding_the_window() {
    let mut s = WindowStack::new();
    let (src, dst) = two_windows(&mut s);
    let mut r = InputRouter::new(SCREEN);

    assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());

    warp(&mut r, &mut s, 450, 20);
    go(&mut r, &mut s, button(true));
    assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());
    assert!(r.start_drag(dst, DROP_KIND_FILE, "/a", "a", &s).is_ok(), "the held one may");
}

#[test]
fn one_grab_carries_one_gesture() {
    let mut s = WindowStack::new();
    let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 20, 20);
    go(&mut r, &mut s, button(true));

    r.start_move(src, &mut s).expect("the grab is held");
    assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err(), "not while moving");

    let mut s = WindowStack::new();
    let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 20, 20);
    go(&mut r, &mut s, button(true));
    r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).expect("the grab is held");
    assert!(r.start_move(src, &mut s).is_err(), "and not the other way round either");
    assert!(r.start_resize(src, RESIZE_RIGHT, &mut s).is_err());
}

#[test]
fn a_window_may_declare_a_bounded_number_of_acceptors() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
    for i in 0..librsproto::surface::MAX_ACCEPTORS {
        let name = alloc::format!("sink{i}");
        s.declare_acceptor(w, &name, DROP_KIND_FILE).expect("within the bound");
    }
    assert_eq!(
        s.declare_acceptor(w, "one-more", DROP_KIND_FILE),
        Err(StackError::TooManyAcceptors),
        "refused rather than evicting one the client still believes in"
    );
    s.declare_acceptor(w, "sink0", DROP_KIND_DIR).expect("replaces");
    assert_eq!(s.acceptor_for(w, DROP_KIND_DIR), Some("sink0"));
}

#[test]
fn a_registered_chord_goes_to_the_manager_and_not_to_the_focused_window() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 7, mods: MOD_SUPER, code: KEY_2 }).unwrap();
    assert_eq!(s.focus_candidate(), Some(w), "precondition: the window has the keyboard");

    let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    assert!(out.is_empty(), "a consumed chord types into nothing");
    assert_eq!(
        r.take_hotkeys(),
        alloc::vec![MgrHotkey { id: 7, mods: MOD_SUPER, code: KEY_2 }],
        "the manager is told which chord fired"
    );
    assert!(r.take_hotkeys().is_empty(), "and told once");

    // The release too: delivering it hands the window a release for a press it never saw.
    let out = go(&mut r, &mut s, chord(KEY_2, false, MOD_SUPER));
    assert!(out.is_empty(), "the release is consumed with the press");
}

#[test]
fn a_chords_release_is_swallowed_by_keycode_even_after_the_modifiers_change() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    let out = go(&mut r, &mut s, chord(KEY_2, false, 0));
    assert!(out.is_empty(), "released after Super came up, and still swallowed");

    let out = go(&mut r, &mut s, key(KEY_2, true));
    assert_eq!(out.len(), 1, "an unmodified press is delivered normally");
}

#[test]
fn modifiers_must_match_exactly() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER | MOD_SHIFT));
    assert_eq!(out.len(), 1, "Super+Shift+2 is not Super+2, so it reaches the window");
    assert!(r.take_hotkeys().is_empty(), "and fires nothing");
}

#[test]
fn an_auto_repeating_chord_does_not_leave_a_release_owed_twice() {
    // One record per press would swallow the next unrelated release of the key too.
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    assert_eq!(r.take_hotkeys().len(), 3, "each press is a chord press");

    assert!(go(&mut r, &mut s, chord(KEY_2, false, MOD_SUPER)).is_empty(), "one release owed");
    let out = go(&mut r, &mut s, key(KEY_2, false));
    assert_eq!(out.len(), 1, "and only one -- a later release is an ordinary keystroke");
}

#[test]
fn a_consumed_chord_tells_the_caller_so_it_does_not_arm_a_repeat() {
    // The binary arms key repeat from the physical transition, so a consumed chord must say so.
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    let mut out = Vec::new();
    let routed = r.route(&chord(KEY_2, true, MOD_SUPER), &mut s, &mut out);
    assert!(routed.consumed, "a chord press must report itself consumed");
    out.clear();
    let routed = r.route(&key(KEY_2, true), &mut s, &mut out);
    assert!(!routed.consumed, "an ordinary press is not");
}

#[test]
fn a_key_already_down_cannot_begin_a_chord() {
    // `2` delivered, then Super held: the repeat would match `Super+2` and swallow the
    // physical release.
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "delivered while unmodified");
    let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    assert_eq!(out.len(), 1, "a key already down is a repeat, not a new chord");
    assert!(r.take_hotkeys().is_empty(), "and it fires nothing");
    assert_eq!(go(&mut r, &mut s, key(KEY_2, false)).len(), 1, "its release is delivered");
}

#[test]
fn dropped_forgets_which_keys_were_consumed() {
    // `Dropped` says held-key state is unknown: a stale record would swallow a later release.
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

    go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    go(&mut r, &mut s, Logical::Dropped { time_ns: T });
    assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "an ordinary press after");
    assert_eq!(
        go(&mut r, &mut s, key(KEY_2, false)).len(),
        1,
        "and its release is delivered rather than swallowed by a stale record"
    );
}

#[test]
fn clearing_the_table_stops_chords_being_consumed_by_nobody() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();
    assert!(go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER)).is_empty(), "consumed");
    r.take_hotkeys();

    r.clear_hotkeys();
    let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
    assert_eq!(out.len(), 1, "with no table the chord is an ordinary keystroke again");
    assert!(r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).is_ok());
}

#[test]
fn registration_refuses_zero_a_duplicate_and_a_full_table() {
    let mut r = InputRouter::new(SCREEN);
    assert_eq!(
        r.register_hotkey(MgrHotkey { id: 0, mods: 0, code: 1 }),
        Err(HotkeyError::ZeroId),
        "zero is reserved so a zeroed body registers nothing"
    );
    r.register_hotkey(MgrHotkey { id: 1, mods: 0, code: 1 }).unwrap();
    assert_eq!(
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: 2 }),
        Err(HotkeyError::DuplicateId),
        "refused rather than silently replaced"
    );
    assert_eq!(
        r.register_hotkey(MgrHotkey { id: 2, mods: 0, code: 1 }),
        Err(HotkeyError::DuplicateChord),
        "a second id for the same chord would be permanently silent"
    );
    for i in 2..=MAX_HOTKEYS as u32 {
        r.register_hotkey(MgrHotkey { id: i, mods: 0, code: i as u16 }).unwrap();
    }
    assert_eq!(r.register_hotkey(MgrHotkey { id: 99, mods: 0, code: 99 }), Err(HotkeyError::TableFull));
}

fn button(pressed: bool) -> Logical {
    Logical::Button { button: BTN_LEFT, pressed, buttons: if pressed { 1 } else { 0 }, modifiers: 0, time_ns: T }
}

#[test]
fn keys_go_to_the_topmost_focusable_window_not_the_one_under_the_cursor() {
    let mut s = WindowStack::new();
    let under = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let top = win(&mut s, Role::Normal, 300, 300, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    assert_eq!(r.inside(), Some(under), "cursor is over the bottom window");

    let out = go(&mut r, &mut s, key(30, true));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].window(), top, "the key went to the focused window");
}

#[test]
fn a_key_carries_its_locks_to_the_window() {
    use librsproto::surface::{LOCK_CAPS, LOCK_NUM};
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    let caps = Logical::Key { keycode: 30, pressed: true, modifiers: 0, locks: LOCK_CAPS | LOCK_NUM, time_ns: T };
    let out = go(&mut r, &mut s, caps);
    match out.as_slice() {
        [Outbound::Key { event }] => assert_eq!((event.window, event.locks), (w, LOCK_CAPS | LOCK_NUM)),
        other => panic!("not one key: {other:?}"),
    }
}

#[test]
fn a_window_whose_configure_is_held_takes_neither_focus_nor_a_click() {
    let mut s = WindowStack::new();
    let visible = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let held = s.create(&CreateWindowRequest::new(200, 200, Role::Normal)).expect("create");

    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    assert_eq!(r.inside(), Some(visible), "the click lands on what is on screen");

    let out = go(&mut r, &mut s, key(30, true));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].window(), visible, "and so does the keystroke");

    s.mark_configured(held);
    warp(&mut r, &mut s, 51, 51);
    assert_eq!(r.inside(), Some(held));
    let out = go(&mut r, &mut s, key(30, true));
    assert_eq!(out[out.len() - 1].window(), held);
}

#[test]
fn a_panel_never_takes_a_keystroke_but_does_take_a_click() {
    let mut s = WindowStack::new();
    let term = win(&mut s, Role::Normal, 0, 100, 640, 380);
    let panel = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 320, 50);

    let out = go(&mut r, &mut s, button(true));
    assert!(
        out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == panel)),
        "the panel got the click"
    );
    go(&mut r, &mut s, button(false));

    let out = go(&mut r, &mut s, key(30, true));
    assert_eq!(out[0].window(), term, "but the keystroke still goes to the terminal");
}

#[test]
fn a_window_on_another_desktop_cannot_be_clicked() {
    let mut s = WindowStack::new();
    let here = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let gone = win(&mut s, Role::Normal, 0, 0, 200, 200);
    s.set_window_desktop(gone, 2).unwrap();
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(&mut r, &mut s, button(true));
    assert!(
        out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == here)),
        "the click went to the window that is actually on screen"
    );
    assert!(
        !out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == gone)),
        "and not to the one on desktop 2"
    );
}

#[test]
fn switching_desktops_mid_drag_takes_the_grab_away_from_the_hidden_window() {
    // The half a filter in `hit()` cannot reach: after a press every event bypasses
    // hit-testing.
    let mut s = WindowStack::new();
    let dragged = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    let out = go(&mut r, &mut s, button(true));
    assert!(
        out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == dragged)),
        "precondition: the press landed and grabbed"
    );

    s.set_current_desktop(2).unwrap();

    let out = go(&mut r, &mut s, drag(10, 10));
    let to_dragged: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Outbound::Pointer { event } if event.window == dragged => Some(event.kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        to_dragged,
        alloc::vec![POINTER_BUTTON, POINTER_LEAVE],
        "the closing release then the leave, in that order and nothing else"
    );
    assert!(
        out.iter().any(|o| matches!(
            o,
            Outbound::Pointer { event }
                if event.window == dragged && event.kind == POINTER_BUTTON
                    && event.flags & POINTER_PRESSED == 0
        )),
        "the synthetic button record is a release, not a press"
    );

    let out = go(&mut r, &mut s, drag(5, 5));
    assert!(out.is_empty(), "no records at all while a broken grab's buttons are down");
    let out = go(&mut r, &mut s, button(false));
    assert!(
        !out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == dragged)),
        "the real release does not reach it either -- it already got its close"
    );
}

#[test]
fn minimizing_the_grab_holder_takes_the_grab_away_too() {
    let mut s = WindowStack::new();
    let dragged = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));

    s.set_minimized(dragged, true).unwrap();

    let out = go(&mut r, &mut s, drag(5, 5));
    let kinds: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Outbound::Pointer { event } if event.window == dragged => Some(event.kind),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, alloc::vec![POINTER_BUTTON, POINTER_LEAVE], "closed, then left");
    assert!(
        !out.iter().any(|o| matches!(
            o,
            Outbound::Pointer { event }
                if event.window == dragged && event.kind == POINTER_MOTION
        )),
        "a minimized window keeps receiving the drag it grabbed"
    );
}

#[test]
fn minimizing_the_window_under_the_cursor_pairs_a_leave_with_the_enter() {
    let mut s = WindowStack::new();
    let under = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let over = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    assert_eq!(r.inside, Some(over), "precondition: the cursor is in the top window");

    s.set_minimized(over, true).unwrap();
    let out = go(&mut r, &mut s, motion(1, 1));

    let leave = out.iter().any(|o| {
        matches!(
        o, Outbound::Pointer { event } if event.window == over && event.kind == POINTER_LEAVE)
    });
    let enter = out.iter().any(|o| {
        matches!(
        o, Outbound::Pointer { event } if event.window == under && event.kind == POINTER_ENTER)
    });
    assert!(leave, "the window that lost the cursor is owed a leave");
    assert!(enter, "and the one that gained it an enter");
}

#[test]
fn breaking_a_grab_gives_the_release_to_nobody_else() {
    let mut s = WindowStack::new();
    let under = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let over = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));

    s.set_minimized(over, true).unwrap();
    let out = go(&mut r, &mut s, button(false));
    assert!(
        !out.iter().any(|o| matches!(
            o, Outbound::Pointer { event } if event.window == under
                && event.kind == POINTER_BUTTON)),
        "the window underneath must not receive a release for a press it never saw"
    );
    assert!(
        out.iter().any(|o| matches!(
            o, Outbound::Pointer { event } if event.window == under
                && event.kind == POINTER_ENTER)),
        "and once the sequence is over it is entered normally"
    );

    let out = go(&mut r, &mut s, button(true));
    assert!(
        out.iter().any(|o| matches!(
            o, Outbound::Pointer { event } if event.window == under
                && event.kind == POINTER_BUTTON)),
        "input resumes after the broken grab's buttons come up"
    );
}

#[test]
fn the_records_a_broken_grab_emits_carry_the_provoking_events_modifiers() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(
        &mut r,
        &mut s,
        Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
    );

    s.set_minimized(w, true).unwrap();
    let out =
        go(&mut r, &mut s, Logical::Button { button: BTN_LEFT, pressed: false, buttons: 0, modifiers: 0, time_ns: T });
    for o in &out {
        let Outbound::Pointer { event } = o else { continue };
        assert_eq!(event.modifiers, 0, "kind {} carries a stale modifier", event.kind);
        assert_eq!(event.buttons, 0, "kind {} carries a stale button mask", event.kind);
    }
    assert!(!out.is_empty(), "the window is owed its close, so there is something to check");
}

#[test]
fn a_sticky_window_is_clickable_on_every_desktop() {
    let mut s = WindowStack::new();
    let bar = win(&mut s, Role::Normal, 0, 0, 200, 200);
    s.set_window_desktop(bar, librsproto::surface::STICKY_DESKTOP).unwrap();
    s.set_current_desktop(7).unwrap();
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(&mut r, &mut s, button(true));
    assert!(
        out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == bar)),
        "a sticky window takes the click on desktop 7"
    );
}

#[test]
fn a_press_outside_a_popup_dismisses_it_and_one_inside_does_not() {
    let mut s = WindowStack::new();
    let bar = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 24 }, 0, 0, 640, 24);
    let menu = win(&mut s, Role::Popup { parent: bar }, 0, 24, 120, 200);
    let mut r = InputRouter::new(SCREEN);
    assert_eq!(s.focus_candidate(), Some(menu), "the popup is topmost and takes focus");

    warp(&mut r, &mut s, 400, 10);
    let mut out = Vec::new();
    let routed = r.route(&button(true), &mut s, &mut out);
    assert_eq!(routed.dismissed, Some(menu), "a press on a panel did not dismiss the popup");
    assert_eq!(routed.restacked, None, "a panel raise would be the other mechanism");

    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 40, 100);
    let routed = r.route(&button(true), &mut s, &mut out);
    assert_eq!(routed.dismissed, None, "pressing the popup dismissed it");

    // Its parent counts as outside, so clicking the opening button again closes the menu.
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 300, 10);
    let routed = r.route(&button(true), &mut s, &mut out);
    assert_eq!(routed.dismissed, Some(menu), "pressing the popup's parent did not dismiss it");
}

#[test]
fn clicking_a_window_raises_it_and_that_is_the_focus_change() {
    let mut s = WindowStack::new();
    let lower = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let upper = win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    assert_eq!(s.focus_candidate(), Some(upper));

    warp(&mut r, &mut s, 50, 50);
    let mut out = Vec::new();
    let routed = r.route(&button(true), &mut s, &mut out);
    // `painted_bounds`, not a literal: the raised window's shadow grows the region.
    let want = s.window(lower).expect("in the stack").painted_bounds();
    assert!(
        want.origin.x < 0 && want.size.w > 100,
        "the premise: a raised window's damage includes its shadow, got {want:?}"
    );
    assert_eq!(routed.restacked, Some(want), "the caller must recompose, and only where the raised window is");
    assert_eq!(s.focus_candidate(), Some(lower), "the raise *is* the focus change");

    let again = r.route(&button(false), &mut s, &mut out);
    assert_eq!(again.restacked, None);
    let again = r.route(&button(true), &mut s, &mut out);
    assert_eq!(again.restacked, None, "already topmost: nothing moved, nothing to paint");
}

#[test]
fn an_interactive_move_offsets_by_where_the_press_landed_not_by_where_the_request_arrived() {
    // An origin taken at the request rather than the press would jump by the pointer's travel.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);

    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    go(&mut r, &mut s, drag(40, 0));
    r.start_move(w, &mut s).expect("the grab is on that window");

    go(&mut r, &mut s, drag(10, 5));
    let origin = s.window(w).expect("still there").origin;
    assert_eq!(
        origin,
        Point::new(150, 105),
        "the window jumped by the distance the pointer travelled before the request"
    );
}

#[test]
fn a_second_start_move_in_one_gesture_changes_nothing() {
    // A second `StartMove` must not rebuild the drag: the origin moved, the press did not.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");
    go(&mut r, &mut s, drag(40, 20));
    let after_first = s.window(w).expect("there").origin;

    r.start_move(w, &mut s).expect("still grabbed");
    assert_eq!(s.window(w).expect("there").origin, after_first, "the second request moved the window again");
}

#[test]
fn a_drag_ends_when_a_dropped_batch_takes_its_grab_away() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");

    go(&mut r, &mut s, Logical::Dropped { time_ns: T });
    let at = s.window(w).expect("there").origin;
    assert_eq!(s.dragging(), None, "the drag went with the grab");
    go(&mut r, &mut s, motion(30, 30));
    assert_eq!(s.window(w).expect("there").origin, at, "and the window stopped following");
    assert!(s.place(w, Point::new(0, 0)).is_ok(), "and a manager may place it again");
}

#[test]
fn a_drag_ends_when_its_window_leaves_the_screen() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");

    s.set_minimized(w, true).expect("minimize");
    go(&mut r, &mut s, drag(30, 30));
    assert_eq!(s.dragging(), None, "the drag went with the grab");
    let at = s.window(w).expect("there").origin;
    go(&mut r, &mut s, drag(30, 30));
    assert_eq!(s.window(w).expect("there").origin, at, "and it stopped following");
}

#[test]
fn a_drag_that_moved_nothing_reports_no_geometry_change() {
    // A click on a title bar is a drag of zero pixels; the manager's queue does not coalesce.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    let _ = s.take_geometry_changes();

    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");
    go(&mut r, &mut s, button(false));
    assert!(s.take_geometry_changes().is_empty(), "the window never moved");
}

#[test]
fn the_catch_up_at_the_start_of_a_drag_reports_its_damage() {
    // A press-flick-release has no motion event to report the damage, so `StartMove` must.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    go(&mut r, &mut s, drag(40, 0));

    let damage = r.start_move(w, &mut s).expect("grabbed").expect("it moved, so it damaged");
    assert!(!damage.is_empty(), "a catch-up that moved the window damaged nothing: {damage:?}");
    assert_eq!(s.window(w).expect("there").origin, Point::new(140, 100));
}

#[test]
fn a_move_needs_the_grab_on_that_window() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);

    assert!(r.start_move(a, &mut s).is_err(), "no button is down at all");

    warp(&mut r, &mut s, 350, 50);
    go(&mut r, &mut s, button(true));
    assert!(r.start_move(a, &mut s).is_err(), "the grab is on the other window");
    assert!(r.start_move(b, &mut s).is_ok(), "and this is the one being pressed");
}

fn go_routed(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Routed {
    let mut out = Vec::new();
    r.route(&ev, s, &mut out)
}

/// The window and rectangle a routed event's ended gesture asks for.
fn asked(r: Routed) -> Option<(u32, Rect)> {
    r.ended.map(|e| (e.window, e.asked))
}

fn grip(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32, edges: u32) {
    warp(r, s, x, y);
    go(r, s, button(true));
    r.start_resize(w, edges, s).expect("the grab is on that window");
}

fn zone(id: u32, t: (i32, i32, u32, u32), g: (i32, i32, u32, u32)) -> MgrSnapZone {
    MgrSnapZone {
        id,
        trigger_x: t.0,
        trigger_y: t.1,
        trigger_w: t.2,
        trigger_h: t.3,
        target_x: g.0,
        target_y: g.1,
        target_w: g.2,
        target_h: g.3,
    }
}

fn hold(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32) {
    warp(r, s, x, y);
    go(r, s, button(true));
    r.start_move(w, s).expect("the grab is on that window");
}

#[test]
fn a_move_into_a_zone_previews_its_target_and_a_drop_asks_for_it() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);

    let routed = go_routed(&mut r, &mut s, drag(-100, 0));
    assert!(routed.outline.is_none(), "nothing is previewed away from a zone");
    assert!(routed.moved.is_some(), "and the move is unaffected");

    let routed = go_routed(&mut r, &mut s, drag(-130, 0));
    let o = routed.outline.expect("the zone is previewed");
    assert_eq!(o.now, Some(Rect::new(0, 0, 320, 480)), "the target, not the trigger");
    assert!(asked(routed).is_none(), "nothing is asked for until the button comes up");

    let routed = go_routed(&mut r, &mut s, button(false));
    assert_eq!(asked(routed), Some((w, Rect::new(0, 0, 320, 480))), "the drop asks for exactly what was previewed");
    assert_eq!(routed.outline.expect("taken down").now, None);
}

#[test]
fn a_drag_that_passes_through_a_zone_without_stopping_snaps_nothing() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);

    go(&mut r, &mut s, drag(-230, 0)); // in
    let out = go_routed(&mut r, &mut s, drag(200, 0)); // and out again
    assert_eq!(out.outline.expect("the preview goes with the pointer").now, None);

    let routed = go_routed(&mut r, &mut s, button(false));
    let now = s.window(w).unwrap().bounds();
    assert_eq!(asked(routed), Some((w, now)), "where the move left it, not the zone's target");
    assert_eq!(now, Rect::new(170, 200, 100, 80));
}

#[test]
fn a_plain_move_reports_where_it_went_and_where_it_came_from() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    hold(&mut r, &mut s, w, 240, 240);
    go(&mut r, &mut s, drag(-50, 30));
    let e = go_routed(&mut r, &mut s, button(false)).ended.expect("a move that went somewhere is reported");
    assert_eq!((e.window, e.asked, e.from), (w, Rect::new(150, 230, 100, 80), Rect::new(200, 200, 100, 80)));
    assert!(e.moved, "already on screen, so nothing is asked");
}

#[test]
fn a_click_on_a_title_bar_reports_no_move() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    hold(&mut r, &mut s, w, 240, 240);
    assert_eq!(go_routed(&mut r, &mut s, button(false)).ended, None);
}

#[test]
fn a_snap_reports_the_rectangle_from_before_the_drag_not_where_the_pointer_took_it() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);
    go(&mut r, &mut s, drag(-230, 0));
    let e = go_routed(&mut r, &mut s, button(false)).ended.expect("a drop in a zone");
    assert_eq!((e.asked, e.from, e.moved), (Rect::new(0, 0, 320, 480), Rect::new(200, 200, 100, 80), false));
}

#[test]
fn a_zone_id_registers_once_and_is_replaced_rather_than_refused() {
    let mut r = InputRouter::new(SCREEN);
    assert_eq!(r.register_zone(zone(0, (0, 0, 1, 1), (0, 0, 1, 1))), Err(HotkeyError::ZeroId));
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    r.register_zone(zone(1, (0, 0, 20, 240), (0, 0, 320, 240))).expect("replaced");
    assert_eq!(r.zone_at(Point::new(5, 300)), None, "the replacement's trigger is what matches, not the original's");
    assert_eq!(r.zone_at(Point::new(5, 100)).map(|z| z.target_h), Some(240));

    for id in 2..=MAX_SNAP_ZONES as u32 {
        r.register_zone(zone(id, (0, 0, 1, 1), (0, 0, 1, 1))).unwrap();
    }
    assert_eq!(
        r.register_zone(zone(99, (0, 0, 1, 1), (0, 0, 1, 1))),
        Err(HotkeyError::TableFull),
        "a new id past the bound is refused"
    );
    r.register_zone(zone(1, (0, 0, 2, 2), (0, 0, 2, 2))).expect("a replacement still fits");
}

#[test]
fn zones_go_with_the_manager_that_registered_them_and_so_does_the_preview() {
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    assert!(r.zone_at(Point::new(5, 5)).is_some());
    assert_eq!(r.clear_zones(), None, "nothing was being previewed");
    assert!(r.zone_at(Point::new(5, 5)).is_none());

    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);
    let shown = go_routed(&mut r, &mut s, drag(-230, 0)).outline.expect("previewed");
    assert_eq!(shown.now, Some(Rect::new(0, 0, 320, 480)));

    let taken = r.clear_zones().expect("the preview is handed back to be repainted");
    assert_eq!(taken.was, Some(Rect::new(0, 0, 320, 480)), "the rectangle to erase");
    assert_eq!(taken.now, None);
    assert!(go_routed(&mut r, &mut s, drag(5, 0)).outline.is_none());
    assert!(go_routed(&mut r, &mut s, button(false)).outline.is_none());
}

#[test]
fn re_registering_the_zone_under_the_pointer_moves_the_preview_with_it() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);
    go(&mut r, &mut s, drag(-230, 0));

    let moved = r
        .register_zone(zone(1, (0, 0, 20, 480), (0, 24, 320, 456)))
        .expect("replaced")
        .expect("the preview moved with it");
    assert_eq!(moved.was, Some(Rect::new(0, 0, 320, 480)));
    assert_eq!(moved.now, Some(Rect::new(0, 24, 320, 456)));
    assert_eq!(
        asked(go_routed(&mut r, &mut s, button(false))),
        Some((w, Rect::new(0, 24, 320, 456))),
        "and the drop asks for the rectangle now on screen"
    );
}

#[test]
fn a_drag_the_user_did_not_finish_snaps_nothing() {
    for interrupted_by_loss in [true, false] {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);
        go(&mut r, &mut s, drag(-230, 0));

        let routed = if interrupted_by_loss {
            go_routed(&mut r, &mut s, Logical::Dropped { time_ns: T })
        } else {
            s.set_minimized(w, true).expect("put away mid-drag");
            go_routed(&mut r, &mut s, key(30, true))
        };
        let e = routed.ended.expect("the move itself is on screen, so it is reported");
        assert!(e.moved, "loss={interrupted_by_loss}: an unfinished drag asked for a snap");
        assert_eq!(e.asked, Rect::new(-30, 200, 100, 80), "loss={interrupted_by_loss}");
        assert_eq!(routed.outline.expect("the preview is taken down").now, None, "loss={interrupted_by_loss}");
    }
}

#[test]
fn a_click_that_shakes_by_less_than_the_threshold_moves_nothing_and_snaps_nothing() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 30, 640, 420);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(7, (0, 0, 640, 54), (0, 30, 640, 450))).unwrap();
    hold(&mut r, &mut s, w, 300, 40);
    let shaken = go_routed(&mut r, &mut s, drag(DRAG_THRESHOLD - 1, 1));
    assert_eq!((shaken.moved, shaken.outline), (None, None), "neither the window nor a zone's preview");
    assert_eq!(go_routed(&mut r, &mut s, button(false)).ended, None);
    assert_eq!(s.window(w).unwrap().bounds(), Rect::new(0, 30, 640, 420));

    hold(&mut r, &mut s, w, 300, 40);
    go(&mut r, &mut s, drag(DRAG_THRESHOLD, 0));
    let e = go_routed(&mut r, &mut s, button(false)).ended.expect("at the threshold it moves");
    assert_eq!(e.asked, Rect::new(0, 30, 640, 450), "and the zone under the pointer takes it");
}

#[test]
fn a_gesture_whose_window_has_gone_asks_for_nothing_even_when_finished() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
    hold(&mut r, &mut s, w, 240, 240);
    go(&mut r, &mut s, drag(-230, 0));
    s.destroy(w).expect("the client exited mid-drag");
    assert_eq!(r.stop_drag(&mut s, true).0, None, "a snap for a window that is not there");

    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
    go(&mut r, &mut s, drag(40, 30));
    s.set_minimized(w, true).expect("put away mid-gesture");
    assert_eq!(r.stop_resize(&mut s, true).0, None, "a resize for a window off screen");
}

#[test]
fn a_zone_previews_only_during_a_move() {
    // The approach must happen inside the assertion: `warp` is itself a motion.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
    let mut r = InputRouter::new(SCREEN);
    r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();

    warp(&mut r, &mut s, 200, 100);
    assert!(
        go_routed(&mut r, &mut s, motion(-190, 0)).outline.is_none(),
        "the pointer crossed into a zone with nothing held, and something was previewed"
    );

    let _ = w;
    // A resize is not asserted: `preview_zone` sits behind `outline_to_pointer`, so such a test
    // passes for wrong code too. `start_move` and `start_resize` refusing each other stops it.
}

#[test]
fn a_resize_needs_the_grab_and_a_pair_of_edges_that_name_a_gesture() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 200, 200);
    let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "no button is down");

    warp(&mut r, &mut s, 350, 50);
    go(&mut r, &mut s, button(true));
    assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "the grab is elsewhere");

    for edges in [0, RESIZE_LEFT | RESIZE_RIGHT, RESIZE_TOP | RESIZE_BOTTOM, 1 << 9] {
        assert_eq!(r.start_resize(b, edges, &mut s), Err(StackError::BadGeometry), "edges {edges:#x}");
    }
    assert!(r.start_resize(b, RESIZE_RIGHT | RESIZE_BOTTOM, &mut s).is_ok());
}

#[test]
fn the_outline_follows_the_pointer_and_the_window_does_not_move() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);

    let routed = go_routed(&mut r, &mut s, drag(40, 30));
    let o = routed.outline.expect("the outline moved");
    assert_eq!(o.now, Some(Rect::new(100, 100, 240, 130)), "both edges followed");
    assert_eq!(o.was, Some(Rect::new(100, 100, 200, 100)), "and the old one is repainted");
    assert_eq!(
        s.window(w).unwrap().bounds(),
        Rect::new(100, 100, 200, 100),
        "the window itself has not changed at all"
    );
    assert!(routed.moved.is_none(), "and nothing was moved");
    assert!(asked(routed).is_none(), "nothing is reported until the button comes up");
}

#[test]
fn dragging_a_top_or_left_edge_moves_the_origin_rather_than_only_the_size() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 100, 100, RESIZE_LEFT | RESIZE_TOP);

    let o = go_routed(&mut r, &mut s, drag(30, 20)).outline.expect("moved");
    assert_eq!(o.now, Some(Rect::new(130, 120, 170, 80)));
}

#[test]
fn an_edge_dragged_past_its_opposite_stops_rather_than_inverting() {
    // The floor is the compositor's own: the protocol cannot state a client's minimum.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);

    let o = go_routed(&mut r, &mut s, drag(-400, -400)).outline.expect("moved");
    assert_eq!(o.now, Some(Rect::new(100, 100, MIN_RESIZE, MIN_RESIZE)));

    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 100, 100, RESIZE_LEFT | RESIZE_TOP);
    let o = go_routed(&mut r, &mut s, drag(400, 400)).outline.expect("moved");
    let now = o.now.unwrap();
    assert_eq!(now.size, libdraw::geom::Size::new(MIN_RESIZE, MIN_RESIZE));
    assert_eq!(now.right(), 300, "the edge that was not dragged did not move");
    assert_eq!(now.bottom(), 200);
}

#[test]
fn the_floor_holds_only_the_axis_being_dragged() {
    // Dragging the bottom edge alone must not widen a window already narrower than the floor.
    let narrow = MIN_RESIZE / 2;
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 10, 10, narrow, 200);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 10 + narrow as i32 - 1, 209, RESIZE_BOTTOM);
    let o = go_routed(&mut r, &mut s, drag(0, -20)).outline.expect("moved");
    assert_eq!(o.now, Some(Rect::new(10, 10, narrow, 180)), "the width is untouched");
}

#[test]
fn the_release_reports_the_rectangle_once_and_takes_the_outline_with_it() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
    go(&mut r, &mut s, drag(40, 30));

    let routed = go_routed(&mut r, &mut s, button(false));
    assert_eq!(asked(routed), Some((w, Rect::new(100, 100, 240, 130))), "one report, carrying where the user let go");
    assert_eq!(routed.ended.unwrap().from, Rect::new(100, 100, 200, 100), "and where it began");
    assert!(!routed.ended.unwrap().moved, "a resize applies nothing");
    let o = routed.outline.expect("the outline is taken down");
    assert_eq!(o.now, None);
    assert_eq!(
        s.window(w).unwrap().bounds(),
        Rect::new(100, 100, 200, 100),
        "and the compositor still has not resized the client"
    );
    assert_eq!(asked(go_routed(&mut r, &mut s, button(false))), None);
}

#[test]
fn a_grip_click_that_moves_nothing_reports_nothing() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
    assert_eq!(asked(go_routed(&mut r, &mut s, button(false))), None);
}

#[test]
fn a_resize_and_a_move_are_one_grab_and_cannot_both_run() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("the move takes the grab");
    assert!(r.start_resize(w, RESIZE_RIGHT, &mut s).is_err(), "one gesture per grab");

    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
    assert!(r.start_move(w, &mut s).is_err(), "one gesture per grab, either order");
    go(&mut r, &mut s, drag(40, 30));
    assert_eq!(s.window(w).unwrap().origin, Point::new(100, 100), "and the window did not move while its outline did");

    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT);
    go(&mut r, &mut s, drag(40, 0));
    assert_eq!(r.start_resize(w, RESIZE_RIGHT, &mut s), Ok(Some(Rect::new(100, 100, 240, 100))));
    assert_eq!(
        asked(go_routed(&mut r, &mut s, button(false))),
        Some((w, Rect::new(100, 100, 240, 100))),
        "the second request named the drag already running"
    );
}

#[test]
fn a_resize_refuses_a_place_and_a_dropped_batch_ends_it_without_committing() {
    // A resize marks the window dragged so a `Place` is refused. `Dropped` ends it but reports
    // nothing: a guessed pointer position must not initiate a change.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
    assert_eq!(s.place(w, Point::new(0, 0)), Err(StackError::Dragging));
    go(&mut r, &mut s, drag(40, 30));

    let routed = go_routed(&mut r, &mut s, Logical::Dropped { time_ns: T });
    assert_eq!(asked(routed), None, "a gesture the user did not finish asks for nothing");
    assert_eq!(routed.outline.expect("taken down").now, None);
    assert!(s.place(w, Point::new(0, 0)).is_ok(), "and the window is a manager's again");
}

#[test]
fn a_window_put_away_mid_resize_ends_the_gesture_on_whichever_key_arm_notices() {
    // `reconcile_with` breaks a grab on any event, so a gesture can end on a key arm; all three
    // are exercised, the hotkey one by releasing the grip after a chord put the window away.
    for arm in ["nothing focusable", "delivered", "hotkey"] {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        if arm != "nothing focusable" {
            win(&mut s, Role::Normal, 400, 0, 100, 100);
        }
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 9, mods: MOD_SUPER, code: KEY_2 }).unwrap();
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        go(&mut r, &mut s, drag(40, 30));
        if arm == "hotkey" {
            let held = go_routed(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
            assert!(held.consumed && held.outline.is_none(), "{arm}: nothing torn down yet");
        }

        s.set_minimized(w, true).expect("put away under the pointer");
        let ev = match arm {
            "hotkey" => chord(KEY_2, false, MOD_SUPER),
            _ => key(30, true),
        };
        let routed = go_routed(&mut r, &mut s, ev);
        assert_eq!(
            routed.outline.expect("the outline is taken down").now,
            None,
            "{arm}: the outline outlived the gesture"
        );
        assert!(asked(routed).is_none(), "{arm}: nothing is asked of the shell for a window that is not on screen");
    }
}

#[test]
fn a_drag_refuses_a_place_and_releases_it_on_the_button_coming_up() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");

    assert_eq!(s.place(w, Point::new(0, 0)), Err(StackError::Dragging));
    go(&mut r, &mut s, button(false));
    assert!(s.place(w, Point::new(0, 0)).is_ok(), "the drag ended with the button");
}

#[test]
fn a_drag_produces_one_geometry_record_however_far_it_moves() {
    // The manager's queue does not coalesce and evicts its oldest, so a record per motion
    // would push out `WindowCreated`.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    let _ = s.take_geometry_changes();

    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");
    for _ in 0..25 {
        go(&mut r, &mut s, drag(3, 2));
    }
    assert!(s.take_geometry_changes().is_empty(), "a drag in flight reports nothing");

    go(&mut r, &mut s, button(false));
    assert_eq!(s.take_geometry_changes(), vec![w], "and exactly one record when it ends");
}

#[test]
fn a_window_destroyed_mid_drag_ends_the_drag_rather_than_wedging_place() {
    // A client exiting with the button held is ordinary; ids are never reused, so a leftover
    // flag would last.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 120, 110);
    go(&mut r, &mut s, button(true));
    r.start_move(w, &mut s).expect("grabbed");

    s.destroy(w).expect("in the stack");
    assert_eq!(s.dragging(), None, "the drag went with the window");
    go(&mut r, &mut s, drag(10, 10)); // must not panic, and nothing to move
}

#[test]
fn clicking_a_panel_does_not_raise_it_over_the_window_it_docks_beside() {
    let mut s = WindowStack::new();
    let panel = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
    let normal = win(&mut s, Role::Normal, 0, 100, 640, 380);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 320, 50);

    let mut out = Vec::new();
    assert_eq!(r.route(&button(true), &mut s, &mut out).restacked, None, "no restack");
    assert_eq!(s.windows().last().map(|w| w.id), Some(normal));
    assert_eq!(s.windows().first().map(|w| w.id), Some(panel));
}

#[test]
fn crossing_a_border_leaves_one_window_before_entering_the_next() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);

    warp(&mut r, &mut s, 50, 50);
    let out = go(&mut r, &mut s, motion(200, 0));
    let kinds: Vec<_> = out
        .iter()
        .map(|o| match o {
            Outbound::Pointer { event } => (event.window, event.kind),
            other => unreachable!("the router emits no {other:?} here"),
        })
        .collect();
    assert_eq!(
        kinds,
        [(a, POINTER_LEAVE), (b, POINTER_ENTER), (b, POINTER_MOTION)],
        "leave, then enter, then the motion that caused both"
    );
}

#[test]
fn every_record_carries_the_buttons_and_modifiers_held() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(
        &mut r,
        &mut s,
        Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
    );
    for o in &out {
        let Outbound::Pointer { event, .. } = o else { unreachable!() };
        assert_eq!((event.buttons, event.modifiers), (1, MOD_SHIFT), "kind {}", event.kind);
    }

    let out = go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0, buttons: 1, modifiers: MOD_SHIFT, time_ns: T });
    let Outbound::Pointer { event, .. } = out
        .iter()
        .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
        .cloned()
        .expect("a motion")
    else {
        unreachable!()
    };
    assert_eq!((event.buttons, event.modifiers), (1, MOD_SHIFT), "a shift-drag is expressible from one record");

    let out = go(
        &mut r,
        &mut s,
        Logical::Button { button: BTN_LEFT, pressed: false, buttons: 0, modifiers: MOD_SHIFT, time_ns: T },
    );
    let entered = out
        .iter()
        .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_ENTER))
        .expect("an enter");
    assert_eq!(entered.window(), b);
    let Outbound::Pointer { event, .. } = entered else { unreachable!() };
    assert_eq!((event.buttons, event.modifiers), (0, MOD_SHIFT));
}

#[test]
fn the_first_click_after_boot_enters_the_window_before_the_press() {
    // Once the press takes the grab the crossing pass early-returns, so the enter must come
    // first.
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 0, 0, 640, 480);
    let mut r = InputRouter::new(SCREEN);
    assert_eq!(r.inside(), None, "no motion since boot");

    let out = go(&mut r, &mut s, button(true));
    let kinds: Vec<_> = out
        .iter()
        .map(|o| match o {
            Outbound::Pointer { event } => (event.window, event.kind),
            other => unreachable!("the router emits no {other:?} here"),
        })
        .collect();
    assert_eq!(kinds, [(w, POINTER_ENTER), (w, POINTER_BUTTON)], "entered, then clicked");
}

#[test]
fn coordinates_are_window_local() {
    let mut s = WindowStack::new();
    let w = win(&mut s, Role::Normal, 200, 100, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 250, 130);

    let out = go(&mut r, &mut s, motion(5, 5));
    let Outbound::Pointer { event } = out
        .iter()
        .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
        .cloned()
        .expect("a motion")
    else {
        unreachable!()
    };
    assert_eq!(event.window, w, "the record names the window it was routed to");
    assert_eq!((event.x, event.y), (55, 35), "screen (255,135) minus origin (200,100)");
}

#[test]
fn a_drag_out_of_a_window_still_delivers_the_release_to_it() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    go(&mut r, &mut s, button(true));
    assert_eq!(r.grab(), Some(a));
    let moved = go(&mut r, &mut s, drag(200, 0));
    assert!(
        s.window(b).expect("b").bounds().contains(r.pointer().x, r.pointer().y),
        "the cursor really did reach b — so the grab is what redirects, not geometry"
    );
    assert!(moved.iter().all(|o| o.window() == a), "everything mid-drag still goes to the grab holder");

    let out = go(&mut r, &mut s, button(false));
    let released = out
        .iter()
        .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_BUTTON))
        .expect("a button record");
    assert_eq!(released.window(), a, "the release went to the window that got the press");
    assert_eq!(r.grab(), None);
}

#[test]
fn window_local_x_goes_negative_during_a_drag() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 250, 50);

    go(&mut r, &mut s, button(true));
    let out = go(&mut r, &mut s, drag(-100, 0));
    let Outbound::Pointer { event, .. } = out
        .iter()
        .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
        .cloned()
        .expect("a motion")
    else {
        unreachable!()
    };
    assert_eq!(event.x, -50, "50px left of the window's left edge");
}

#[test]
fn the_release_re_derives_the_crossing_the_drag_suppressed() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));
    go(&mut r, &mut s, drag(200, 0));
    assert_eq!(r.inside(), Some(a), "suppressed while grabbed: still a, though over b");

    let out = go(&mut r, &mut s, button(false));
    assert_eq!(r.inside(), Some(b));
    let crossings: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Outbound::Pointer { event } if event.kind == POINTER_LEAVE || event.kind == POINTER_ENTER => {
                Some((event.window, event.kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(crossings, [(a, POINTER_LEAVE), (b, POINTER_ENTER)]);
}

#[test]
fn a_dropped_batch_ends_the_grab() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));
    go(&mut r, &mut s, drag(200, 0));
    assert_eq!(r.grab(), Some(a));

    go(&mut r, &mut s, Logical::Dropped { time_ns: T });
    assert_eq!(r.grab(), None);
    let out = go(&mut r, &mut s, motion(1, 0));
    assert!(out.iter().all(|o| o.window() == b), "later events follow the cursor again");
}

#[test]
fn destroying_the_grab_holder_does_not_route_to_a_dead_id() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));
    assert_eq!(r.grab(), Some(a));

    s.destroy(a).expect("destroy");
    let out = go(&mut r, &mut s, drag(200, 0));
    assert!(out.iter().all(|o| o.window() == b), "nothing addressed to the dead window");
    assert_eq!(r.grab(), None);
}

#[test]
fn the_cursor_stops_at_the_screen_edge() {
    let mut s = WindowStack::new();
    let mut r = InputRouter::new(SCREEN);
    go(&mut r, &mut s, motion(-10_000, -10_000));
    assert_eq!(r.pointer(), Point::new(0, 0));
    go(&mut r, &mut s, motion(10_000, 10_000));
    assert_eq!(r.pointer(), Point::new(639, 479), "the last addressable pixel, not 640");
}

#[test]
fn a_mouse_held_against_the_edge_does_not_wrap_to_the_other_side() {
    // Rightward can overflow: `639 + i32::MAX` wraps negative and pins to the left edge.
    let mut s = WindowStack::new();
    let mut r = InputRouter::new(SCREEN);
    for _ in 0..3 {
        go(&mut r, &mut s, motion(i32::MAX, i32::MAX));
    }
    assert_eq!(r.pointer(), Point::new(639, 479), "still pinned to the far edge");
}

#[test]
fn a_key_with_nothing_focusable_is_dropped_rather_than_sent_to_the_cursor() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 320, 50);
    assert!(go(&mut r, &mut s, key(30, true)).is_empty());
}

#[test]
fn the_press_that_focuses_a_window_is_also_delivered_to_it() {
    let mut s = WindowStack::new();
    let lower = win(&mut s, Role::Normal, 0, 0, 640, 480);
    win(&mut s, Role::Normal, 300, 300, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(&mut r, &mut s, button(true));
    assert!(out.iter().all(|o| o.window() == lower));
    assert_eq!(r.grab(), Some(lower));
}

fn wheel(dz: i32) -> Logical {
    Logical::Wheel { dz, buttons: 0, modifiers: 0, time_ns: T }
}

#[test]
fn a_wheel_reaches_the_window_under_the_pointer() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 350, 50);

    let out = go(&mut r, &mut s, wheel(2));
    assert_eq!(out.len(), 1, "a wheel moves no cursor, so it crosses nothing");
    let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
    assert_eq!(event.window, b, "the window under the cursor, not {a}");
    assert_eq!(event.kind, POINTER_WHEEL);
    assert_eq!(event.wheel, 2);
    assert_eq!((event.x, event.y), (50, 50), "and window-local, like every other record");
}

#[test]
fn a_wheel_neither_raises_nor_focuses() {
    let mut s = WindowStack::new();
    let lower = win(&mut s, Role::Normal, 0, 0, 640, 480);
    let upper = win(&mut s, Role::Normal, 300, 300, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    assert_eq!(s.focus_candidate(), Some(upper), "precondition: the small one is on top");

    let routed = go_routed(&mut r, &mut s, wheel(1));
    assert_eq!(routed.restacked, None, "scrolling reordered the screen");
    assert_eq!(s.focus_candidate(), Some(upper), "…and moved the keyboard with it");
    assert_eq!(r.grab(), None, "a wheel is not a press and opens no grab");
    let _ = lower;
}

#[test]
fn a_wheel_during_a_drag_goes_to_the_grab_holder() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);
    go(&mut r, &mut s, button(true));
    go(&mut r, &mut s, drag(300, 0));
    assert_eq!(r.grab(), Some(a), "precondition: the press grabbed the first window");

    let out = go(&mut r, &mut s, wheel(-1));
    assert!(out.iter().all(|o| o.window() == a), "the sequence belongs to the grab holder");
}

#[test]
fn a_wheel_carries_the_state_held_while_it_turned() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(&mut r, &mut s, Logical::Wheel { dz: 1, buttons: 0, modifiers: MOD_SHIFT, time_ns: T });
    let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
    assert_eq!(event.modifiers, MOD_SHIFT);
    assert_eq!(event.time_ms, T_MS);
}

/// A delta too large for the wire saturates rather than wrapping.
#[test]
fn a_wheel_delta_too_large_for_the_wire_saturates() {
    let mut s = WindowStack::new();
    win(&mut s, Role::Normal, 0, 0, 200, 200);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 50, 50);

    let out = go(&mut r, &mut s, wheel(i32::from(i16::MAX) + 10));
    let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
    assert_eq!(event.wheel, i16::MAX, "saturated, still downward");

    let out = go(&mut r, &mut s, wheel(i32::MIN));
    let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
    assert_eq!(event.wheel, i16::MIN);
}

/// Every record of a batch carries the interrupt's time, synthesised crossings included.
#[test]
fn every_record_carries_the_time_of_the_event_that_caused_it() {
    let mut s = WindowStack::new();
    let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
    let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
    let mut r = InputRouter::new(SCREEN);

    let at = r.pointer();
    let out = go(&mut r, &mut s, motion(50 - at.x, 50 - at.y));
    let times: Vec<(u16, u64)> = out
        .iter()
        .filter_map(|o| match o {
            Outbound::Pointer { event } => Some((event.kind, event.time_ms)),
            _ => None,
        })
        .collect();
    assert!(times.iter().any(|(k, _)| *k == POINTER_ENTER), "the cursor entered {a}");
    assert!(
        times.iter().all(|&(_, t)| t == T_MS),
        "a record was stamped from somewhere other than the event: {times:?}"
    );

    let out = go(&mut r, &mut s, motion(300, 0));
    let leave = out
        .iter()
        .find_map(|o| match o {
            Outbound::Pointer { event } if event.kind == POINTER_LEAVE => Some(*event),
            _ => None,
        })
        .expect("left the first window");
    assert_eq!(leave.window, a);
    assert_eq!(leave.time_ms, T_MS);
    assert!(out.iter().any(|o| o.window() == b), "and entered the second");
}
