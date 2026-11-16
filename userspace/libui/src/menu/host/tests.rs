use super::*;

use alloc::rc::Rc;
use alloc::vec;
use core::cell::{Cell, RefCell};

use libdraw::geom::Rect;
use librsproto::surface::{
    ConfigureEvent, CreateWindowRequest, OP_CONFIGURE, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_DISMISSED,
    OP_POINTER_EVENT, POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent, WindowRef,
    parse_create_window_request,
};
use libsurface::UiError;
use libsurface::mock::MockTransport;

use crate::menu::{Item, Menu, MenuState, popup};

const DEJAVU: &[u8] = include_bytes!("../../../../../assets/fonts/DejaVuSans.ttf");

/// The mock, shared, so a test can read what was sent and queue what arrives while a `Session`
/// owns the transport; every request counted, refused ones included; and a window configured at
/// the offset it asked for, where the mock says `(0, 0)`, so an origin can be told from its swap.
#[derive(Clone, Default)]
struct Shared(Rc<RefCell<MockTransport>>, Rc<Cell<usize>>);

impl Transport for Shared {
    fn request(
        &mut self,
        op: u16,
        body: &[u8],
        handle: Option<u64>,
        reply: &mut [u8],
    ) -> Result<Option<usize>, UiError> {
        self.1.set(self.1.get() + 1);
        let answer = self.0.borrow_mut().request(op, body, handle, reply);
        if op == OP_CREATE_WINDOW
            && let Some(req) = parse_create_window_request(body)
            && let Some((OP_CONFIGURE, cfg)) = self.0.borrow_mut().events.first_mut()
            && let Some(mut c) = ConfigureEvent::read(cfg)
        {
            (c.x, c.y) = (req.offset_x, req.offset_y);
            c.write(cfg);
        }
        answer
    }
    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        self.0.borrow_mut().poll_event(buf)
    }
    fn took_loss(&mut self) -> bool {
        self.0.borrow_mut().took_loss()
    }
    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        self.0.borrow_mut().wait_event(buf)
    }
}

fn bar() -> Vec<Menu<u32>> {
    vec![
        Menu { title: "File", items: vec![Item::plain("New", 1), Item::Separator, Item::plain("Quit", 2)] },
        Menu { title: "Edit", items: vec![Item::plain("Copy", 3)] },
    ]
}

fn context() -> Menu<u32> {
    Menu { title: "", items: vec![Item::plain("Open", 10), Item::plain("Rename", 11)] }
}

fn view(state: &MenuState, shown: Shown, hovered: Option<u64>, theme: &Theme) -> Element<u32> {
    match shown {
        Shown::Bar(i) => popup(&bar()[i], state, 100, hovered, theme),
        Shown::Context(_) => popup(&context(), state, 100, hovered, theme),
    }
}

/// A window, its menus and their host, on the shared mock.
struct Rig {
    t: Shared,
    s: Session<Shared>,
    parent: u32,
    host: Host,
    state: MenuState,
    font: Font,
    theme: Theme,
}

impl Rig {
    fn new() -> Self {
        let t = Shared::default();
        let mut s = Session::new(t.clone());
        let parent = s.create(&CreateWindowRequest::new(400, 300, Role::Normal), 2).expect("the window");
        let mut state = MenuState::new(2);
        state.set_anchors(vec![Some(Rect::new(10, 0, 40, 20)), Some(Rect::new(60, 0, 40, 20))]);
        let font = Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses");
        Self { t, s, parent, host: Host::new(2), state, font, theme: Theme::default() }
    }

    fn sync(&mut self) -> Synced {
        let (state, theme) = (self.state.clone(), self.theme);
        let want = self.state.shown_at();
        self.host.sync(
            &mut self.s,
            self.parent,
            want,
            |shown, h| view(&state, shown, h, &theme),
            &self.font,
            &self.theme,
        )
    }

    fn event(&mut self, e: &WindowEvent) -> Routed<u32> {
        let (state, theme) = (self.state.clone(), self.theme);
        self.host.event(e, |shown, h| view(&state, shown, h, &theme), &self.font, &self.theme)
    }

    /// The popups asked for, in order: every `CreateWindow` after the window's own.
    fn popups(&self) -> Vec<CreateWindowRequest> {
        let sent = &self.t.0.borrow().sent;
        sent.iter()
            .filter(|(op, ..)| *op == OP_CREATE_WINDOW)
            .skip(1)
            .filter_map(|(_, b, _)| parse_create_window_request(b))
            .collect()
    }

    fn destroys(&self) -> usize {
        self.t.0.borrow().sent.iter().filter(|(op, ..)| *op == OP_DESTROY_WINDOW).count()
    }
}

#[test]
fn a_drop_down_opens_inside_and_another_menu_rebuilds_the_window() {
    let mut r = Rig::new();
    r.state.toggle(0);
    let file = r.sync().opened.expect("File opened");
    assert_eq!(file.1, (10, 20), "the origin it was configured at");
    let asked = r.popups();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].place, asked[0].offset_x, asked[0].offset_y), (Placement::Inside, 10, 20), "under its word");

    r.state.toggle(1);
    let edit = r.sync().opened.expect("Edit opened");
    assert_eq!((r.popups().len(), r.destroys()), (2, 1), "File's window went, and Edit's came");
    assert_ne!(file.0, edit.0);
    assert!(edit.2.h < file.2.h, "Edit's one row in a window of its own size: {edit:?} after {file:?}");
}

#[test]
fn the_window_closes_when_nothing_is_wanted_and_asks_for_nothing_more() {
    let mut r = Rig::new();
    r.state.toggle(0);
    r.sync();
    r.state.close();
    r.sync();
    assert_eq!((r.popups().len(), r.destroys(), r.host.shown()), (1, 1, None));
    let before = r.t.1.get();
    r.sync();
    assert_eq!(r.t.1.get(), before, "a closed menu asks the compositor for nothing");
}

#[test]
fn one_menu_at_a_time_and_a_context_menu_asks_to_be_placed_at_its_point() {
    let mut r = Rig::new();
    r.state.toggle(0);
    r.sync();
    r.state.open_context(120, 90);
    assert_eq!(r.state.open(), None, "opening a context menu closed the bar's");
    r.sync();
    let last = *r.popups().last().expect("a popup");
    assert_eq!((last.place, last.offset_x, last.offset_y), (Placement::AtPoint, 120, 90));
    assert_eq!(r.destroys(), 1, "File's window went first");

    r.state.toggle(1);
    assert_eq!(r.state.context(), None, "opening a bar menu closed the context menu");
    r.sync();
    assert_eq!(r.popups().last().map(|p| p.place), Some(Placement::Inside));
}

#[test]
fn a_dismissed_closes_only_the_menu_its_window_showed() {
    let mut r = Rig::new();
    r.state.open_context(10, 10);
    r.sync();
    let Routed::Dismissed(shown) = r.event(&WindowEvent::Dismissed) else { panic!("not a dismissal") };
    r.state.open_context(50, 50);
    r.state.dismissed(shown);
    assert_eq!(r.state.context(), Some((50, 50)), "the menu the press opened survived the old one's dismissal");
    r.sync();
    let Routed::Dismissed(now) = r.event(&WindowEvent::Dismissed) else { panic!("not a dismissal") };
    r.state.dismissed(now);
    assert_eq!(r.state.shown(), None, "and the one on show closes");
}

/// A right press elsewhere in the window moves the menu, whichever of the press and the
/// `Dismissed` its owner reads first: the session hands out the window's press first, though the
/// wire carries the `Dismissed` first.
#[test]
fn a_right_press_elsewhere_moves_the_menu_in_either_order() {
    let mut r = Rig::new();
    r.state.open_context(10, 10);
    r.sync();
    let popup_id = r.host.child.as_ref().expect("up").id();
    let press = PointerEvent {
        window: r.parent,
        kind: POINTER_BUTTON,
        button: libkern::abi::BTN_RIGHT,
        buttons: 2,
        flags: POINTER_PRESSED,
        x: 200,
        y: 150,
        ..Default::default()
    };
    {
        let mut b = [0u8; 64];
        let mut m = r.t.0.borrow_mut();
        let n = WindowRef { window: popup_id }.write(&mut b).expect("encodes");
        m.events.insert(0, (OP_DISMISSED, b[..n].to_vec()));
        let n = press.write(&mut b).expect("encodes");
        m.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
    }
    r.s.pump().expect("pumps");
    let mut order = Vec::new();
    while let Some((from, e)) = r.s.next_event() {
        if from == r.parent && matches!(e, WindowEvent::Pointer(p) if p.flags & POINTER_PRESSED != 0) {
            order.push("press");
            r.state.open_context(200, 150);
        } else if r.host.owns(from) {
            order.push("dismissed");
            if let Routed::Dismissed(shown) = r.event(&e) {
                r.state.dismissed(shown);
            }
        }
    }
    assert_eq!(order, ["press", "dismissed"], "the session's order");
    r.sync();
    assert_eq!(r.popups().last().map(|p| (p.offset_x, p.offset_y)), Some((200, 150)), "moved, in the session's order");

    // The wire's order, the other way round.
    let Routed::Dismissed(shown) = r.event(&WindowEvent::Dismissed) else { panic!("not a dismissal") };
    r.state.dismissed(shown);
    r.state.open_context(30, 40);
    r.sync();
    assert_eq!(r.popups().last().map(|p| (p.offset_x, p.offset_y)), Some((30, 40)), "moved, in the wire's order");
}

#[test]
fn a_failed_open_is_answered_once() {
    let mut r = Rig::new();
    r.state.toggle(0);
    r.t.0.borrow_mut().fail = true;
    let before = r.t.1.get();
    assert!(r.sync().failed, "the open failed");
    assert_eq!(r.t.1.get(), before + 1, "one CreateWindow");
    // What the owner does with `failed`: close its state, so nothing asks again.
    r.state.close();
    r.sync();
    r.sync();
    assert_eq!(r.t.1.get(), before + 1, "and none after it");
}

/// A widget can be present and draw nothing: the popup's frame is counted for ink.
#[test]
fn the_popup_draws_its_rows() {
    let mut r = Rig::new();
    r.state.toggle(0);
    assert!(!r.sync().undrawn);
    let frame = r.host.child.as_ref().expect("up").frame();
    let px: Vec<&[u8; 4]> = frame.as_chunks::<4>().0.iter().collect();
    let ground = px[px.len() / 2 + 2];
    let ink = px.iter().filter(|p| **p != ground).count();
    assert!(ink > 100, "{ink} pixels differ from the ground: the rows were not drawn");
}

/// The row under the pointer is reported once as it changes, the popup is drawn with it, and a
/// press routes against that tree; a window opened again starts with nothing under the pointer.
#[test]
fn the_row_under_the_pointer_is_reported_drawn_and_routed_against() {
    let mut r = Rig::new();
    r.state.toggle(0);
    r.sync();
    let popup = r.host.child.as_ref().expect("up").id();
    let at = |kind, buttons, flags| PointerEvent {
        window: popup,
        kind,
        button: libkern::abi::BTN_LEFT,
        buttons,
        flags,
        x: 20,
        y: 16,
        ..Default::default()
    };
    assert_eq!(r.event(&WindowEvent::Pointer(at(POINTER_MOTION, 0, 0))), Routed::Msgs(vec![]));
    assert_eq!(r.sync().hover, Some(Some(100)), "the pointer is over New, row key 100");
    assert_eq!(r.sync().hover, None, "and said once");
    assert_eq!(r.event(&WindowEvent::Pointer(at(POINTER_BUTTON, 1, POINTER_PRESSED))), Routed::Msgs(vec![]));
    assert_eq!(r.event(&WindowEvent::Pointer(at(POINTER_BUTTON, 0, 0))), Routed::Msgs(vec![1]), "a click on New");

    r.state.close();
    r.sync();
    r.state.toggle(0);
    assert_eq!(r.sync().hover, None, "a window opened again reports no change from the last one's row");
}

#[test]
fn the_host_owns_only_its_popup_and_hands_its_keys_on() {
    let mut r = Rig::new();
    assert!(!r.host.owns(r.parent), "no popup yet");
    r.state.toggle(0);
    r.sync();
    let popup = r.host.child.as_ref().expect("up").id();
    assert!(r.host.owns(popup));
    assert!(!r.host.owns(r.parent) && !r.host.owns(popup + 1), "nor any other window");
    let k = KeyEvent::new(popup, libkern::abi::KEY_DOWN, 1, 0);
    assert_eq!(r.event(&WindowEvent::Key(k)), Routed::Key(k));
}

/// The press, its dismissal and its release in one pump, as the compositor now sends them: the
/// session hands out the window's two events first, and a dismissal read after the release holds
/// back no later click on the word.
#[test]
fn a_right_press_outside_and_its_release_in_one_pump_hold_back_no_later_click() {
    let mut r = Rig::new();
    r.state.toggle(0);
    r.sync();
    let popup = r.host.child.as_ref().expect("up").id();
    let ev = |buttons, flags| PointerEvent {
        window: r.parent,
        kind: POINTER_BUTTON,
        button: libkern::abi::BTN_RIGHT,
        buttons,
        flags,
        x: 300,
        y: 200,
        ..Default::default()
    };
    {
        let mut b = [0u8; 64];
        let mut m = r.t.0.borrow_mut();
        let n = ev(2, POINTER_PRESSED).write(&mut b).expect("encodes");
        m.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
        let n = WindowRef { window: popup }.write(&mut b).expect("encodes");
        m.events.insert(0, (OP_DISMISSED, b[..n].to_vec()));
        let n = ev(0, 0).write(&mut b).expect("encodes");
        m.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
    }
    r.s.pump().expect("pumps");
    while let Some((from, e)) = r.s.next_event() {
        match e {
            WindowEvent::Pointer(p) if from == r.parent => r.state.buttons(p.buttons),
            e if r.host.owns(from) => {
                if let Routed::Dismissed(shown) = r.event(&e) {
                    r.state.dismissed(shown);
                }
            }
            _ => {}
        }
    }
    assert_eq!(r.state.shown(), None, "dismissed");
    r.sync();
    r.state.buttons(1);
    r.state.toggle(0);
    assert_eq!(r.state.open(), Some(0), "a click on File after it opened File");
}
