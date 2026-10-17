//! `desktop-session-mgr`'s host-testable internals: the greeter's state and the window it draws.
//! `GREETER_W`/`GREETER_H` are read by a test in `xtask`, which keeps `check-login`'s copy equal.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libui::element::{
    Element, Insets, TextSize, bold, center_v, column, ink, padding, row, scaled, sized, text, with_spacing,
};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};
use libui::widget::{TextFieldState, WidgetState, popup_frame, text_field};

/// The greeter window's size; only its position follows the screen (see `_start`).
///
/// The height is the card's measured one, held by
/// `the_card_is_exactly_the_window_it_is_drawn_in`. The width is a choice: the fields flex, so
/// the card is not as wide as its content.
pub const GREETER_W: u32 = 340;
/// See [`GREETER_W`].
pub const GREETER_H: u32 = 141;
/// Bytes per row, unpadded, so the buffer copy is a memcpy.
pub const GREETER_PITCH: usize = (GREETER_W as usize) * 4;
const CARD_PAD: u32 = 18;
const ROW_GAP: u32 = 8;
/// A field's height: the chooser's, so a box to type in is one size across the system.
const FIELD_H: u32 = 24;
/// The label column, fixed so both labels share an edge.
const LABEL_W: u32 = 74;
/// The username row's label and field keys, so a test can locate the field.
const USER_KEYS: (u64, u64) = (1, 2);
const PASSWORD_KEYS: (u64, u64) = (3, 4);
const REFUSAL_H: u32 = 16;

/// The theme the greeter draws with and loads its font from: always the built-in one, because a
/// theme lives in a user's home and this runs before there is a user.
pub fn greeter_theme() -> Theme {
    Theme::default()
}

/// Which field the keyboard is going to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    /// The username field.
    User,
    /// The password field.
    Password,
}

/// Everything the greeter draws from.
pub struct Greeter {
    /// The username being typed.
    pub user: TextFieldState,
    /// The password being typed. Rendered masked.
    pub password: TextFieldState,
    /// Which field has the caret.
    focus: Focus,
    /// Whether the last attempt was refused.
    pub denied: bool,
}

/// `EV_KEY` codes the greeter acts on itself; [`TextFieldState::apply`] declines them.
pub const KEY_TAB: u16 = 15;
/// See [`KEY_TAB`].
pub const KEY_ENTER: u16 = 28;

impl Default for Greeter {
    fn default() -> Self {
        Self::new()
    }
}

impl Greeter {
    /// An empty greeter, caret in the username field.
    pub fn new() -> Self {
        Self { user: TextFieldState::new(), password: TextFieldState::new(), focus: Focus::User, denied: false }
    }

    /// The element tree for the current state, in the theme the caller paints it with.
    pub fn view(&self, theme: &Theme) -> Element<()> {
        let active = |f: Focus| WidgetState { active: self.focus == f, ..Default::default() };
        let field = |f: &TextFieldState, masked, which| {
            sized(Size::new(0, FIELD_H), text_field(f, masked, active(which), theme))
        };
        // The label and its field both carry a key: a container's children are all keyed or none.
        let labelled = |label: &str, f: Element<()>, keys: (u64, u64)| {
            row(alloc::vec![
                sized(Size::new(LABEL_W, FIELD_H), center_v(ink(theme.foreground_dim, text(label))),).key(keys.0),
                f.flex(1).key(keys.1),
            ])
        };
        let mut rows = alloc::vec::Vec::with_capacity(4);
        rows.push(bold(scaled(TextSize::Large, text("nitrox"))));
        // Always present and empty until there is a refusal, so the fields never move under the
        // caret in a window of fixed size.
        let refusal = if self.denied { "login incorrect" } else { "" };
        rows.push(sized(Size::new(0, REFUSAL_H), center_v(ink(theme.deny, scaled(TextSize::Small, text(refusal))))));
        rows.push(labelled("username", field(&self.user, false, Focus::User), USER_KEYS));
        let password = field(&self.password, true, Focus::Password);
        rows.push(labelled("password", password, PASSWORD_KEYS));
        popup_frame(padding(Insets::all(CARD_PAD), with_spacing(column(rows), ROW_GAP)), theme)
    }

    fn active_field(&mut self) -> &mut TextFieldState {
        match self.focus {
            Focus::User => &mut self.user,
            Focus::Password => &mut self.password,
        }
    }

    /// Apply a key. `true` if anything changed and the greeter must be redrawn.
    ///
    /// Tab is handled here, not by the field: a field that swallowed it could never be left.
    pub fn key(&mut self, keycode: u16, modifiers: u16, locks: u16) -> bool {
        match keycode {
            KEY_TAB => {
                self.focus = match self.focus {
                    Focus::User => Focus::Password,
                    Focus::Password => Focus::User,
                };
                true
            }
            _ => {
                // An edit clears a previous refusal, which would otherwise read as a second one.
                let changed = self.active_field().apply(keycode, modifiers, locks);
                if changed && self.denied {
                    self.denied = false;
                }
                changed
            }
        }
    }

    /// Clear both fields and put the caret back, after a session ends and after a refusal.
    ///
    /// The password leaves the screen the moment it has been read, whichever way the attempt
    /// went, so none sits in the field behind a session. Not a scrub: `String::clear` leaves the
    /// bytes in the allocation, so this is a claim about what is displayed, not about memory.
    /// The caller's stack copy is volatile-zeroed after the session ends.
    pub fn reset(&mut self) {
        self.user.clear();
        self.password.clear();
        self.focus = Focus::User;
    }

    /// Render the current state into a fresh framebuffer.
    pub fn render(&self, font: &Font) -> MemFramebuffer {
        let geometry = Geometry::with_pitch(GREETER_W, GREETER_H, GREETER_PITCH, PixelFormat::XRGB8888)
            .expect("the greeter pitch is wide enough for a row");
        let mut fb = MemFramebuffer::new(geometry);
        // One theme for the tree and the paint.
        let theme = greeter_theme();
        let ui = self.view(&theme);
        let bounds = Rect::new(0, 0, GREETER_W, GREETER_H);
        let metrics = FontMetrics::new(font, theme.font_px);
        let l = layout(&ui, bounds, &metrics);
        paint(&mut fb, font, &theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::framebuffer::Framebuffer;

    const MIN_FIELD_W: u32 = 200;

    /// `EV_KEY` for the `a` key, a keystroke that edits.
    const KEY_A: u16 = 30;

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
    const BOLD: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans-Bold.ttf");

    /// The faces the greeter draws with, as `load_ui` attaches them in the guest.
    fn faces() -> Font {
        Font::from_bytes(DEJAVU.to_vec())
            .expect("the vendored face parses")
            .with_bold(Font::from_bytes(BOLD.to_vec()).expect("its bold parses"))
    }

    #[test]
    fn tab_moves_the_caret_between_the_fields() {
        let mut g = Greeter::new();
        assert_eq!(g.focus, Focus::User, "the caret starts where the typing does");
        assert!(g.key(KEY_TAB, 0, 0), "Tab changed something, so the greeter must redraw");
        assert_eq!(g.focus, Focus::Password);
        assert!(g.key(KEY_TAB, 0, 0));
        assert_eq!(g.focus, Focus::User, "and Tab cycles rather than stopping at the end");
        let before = g.focus;
        assert!(g.key(KEY_A, 0, 0));
        assert_eq!(g.focus, before, "typing does not move the caret between fields");
        assert_eq!(g.user.text(), "a", "…it goes into the field the caret is in");
        assert_eq!(g.password.text(), "", "and only that one");
    }

    #[test]
    fn an_edit_clears_a_refusal_and_reset_clears_the_greeter() {
        let mut g = Greeter::new();
        g.denied = true;
        assert!(g.key(KEY_A, 0, 0), "the edit landed");
        assert!(!g.denied, "and it took the refusal with it");
        g.denied = true;
        assert!(g.key(KEY_TAB, 0, 0));
        assert!(g.denied, "moving the caret is not an edit");
        assert_eq!(g.focus, Focus::Password);
        assert!(g.key(KEY_A, 0, 0));
        assert_eq!((g.user.text(), g.password.text()), ("a", "a"), "both fields hold something");
        g.reset();
        assert_eq!((g.user.text(), g.password.text()), ("", ""), "nothing typed is left behind");
        assert_eq!(g.focus, Focus::User, "and the caret is back where a login starts");
    }

    /// Painted, because a tree inspection passes against a refusal in the body colour too.
    #[test]
    fn a_refusal_is_drawn_in_the_deny_colour() {
        let (f, t) = (faces(), greeter_theme());
        let reddish = |g: &Greeter| {
            let geometry = Geometry::packed(GREETER_W, GREETER_H, PixelFormat::XRGB8888);
            let mut fb = MemFramebuffer::new(geometry);
            fb.clear(t.background);
            let e = g.view(&t);
            let bounds = Rect::new(0, 0, GREETER_W, GREETER_H);
            let l = libui::layout::layout(&e, bounds, &FontMetrics::new(&f, t.font_px));
            paint(&mut fb, &f, &t, &e, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
            let mut n = 0;
            for y in 0..GREETER_H {
                for x in 0..GREETER_W {
                    // A red cast, not the exact colour: antialiasing leaves few pixels at `deny`
                    // itself, and every other colour on this card is a grey, white or teal.
                    if let Some(c) = fb.get_pixel(x, y)
                        && c.r > c.g.saturating_add(20)
                        && c.r > c.b.saturating_add(20)
                    {
                        n += 1;
                    }
                }
            }
            n
        };
        let mut g = Greeter::new();
        assert_eq!(reddish(&g), 0, "nothing is refused yet, so the line says nothing");
        g.denied = true;
        assert!(reddish(&g) > 20, "a refusal is drawn, in the colour a refusal is drawn in");
        assert_ne!(t.deny, t.foreground, "…which is not the ink of everything else");
        assert!(t.deny.r > t.deny.g && t.deny.r > t.deny.b, "and `deny` is what leans red here");
    }

    /// `xtask`'s `the_gates_greeter_size_is_the_greeters_own` holds `check-login`'s copy of the
    /// size equal to the constants this compares the card with.
    #[test]
    fn the_card_is_exactly_the_window_it_is_drawn_in() {
        let f = faces();
        let t = greeter_theme();
        let m = FontMetrics::new(&f, t.font_px);
        let measure = |g: &Greeter| {
            libui::layout::measure(&g.view(&t), libui::layout::Constraints::loose(Size::new(4000, 4000)), &m)
        };
        let mut g = Greeter::new();
        assert_eq!(measure(&g).h, GREETER_H, "the window is the height of what is in it");
        g.denied = true;
        assert_eq!(measure(&g).h, GREETER_H, "a refusal changes nothing about the card's height");
        // `measure` cannot size a flexing row, so lay the card out at the window's size instead.
        let bounds = Rect::new(0, 0, GREETER_W, GREETER_H);
        let e = g.view(&t);
        let l = libui::layout::layout(&e, bounds, &m);
        let field = libui::layout::locate(&e, &l, USER_KEYS.1).expect("the username field");
        assert!(field.size.w >= MIN_FIELD_W, "a field laid out {} wide is too narrow to type in", field.size.w);
        assert!(
            field.origin.x + field.size.w as i32 <= (GREETER_W - CARD_PAD) as i32,
            "the field runs past the card's padding: {field:?}"
        );
    }
}
