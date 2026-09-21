//! `desktop-session-mgr`'s host-testable internals: the greeter's state and the window it draws.
//!
//! **A library beside the binary, holding only what is pure.** The supervisor half is built
//! around syscalls and cannot be host-tested at all; the greeter's state, its key handling and
//! its element tree need no world, and until the desktop refresh's Part D they were untested
//! because there was nowhere to test them from. `init`, `service-mgr`, `nxterm`, `nxfiles`,
//! `nxedit` and `desktop-shell` each grew a library for this reason; this is the same move, and
//! it is what lets the size `check-login` writes down be measured rather than asserted.
//!
//! `#![no_std]` for the bare build; `std` under `cargo test` so the host harness works
//! (`cargo xtask test` runs `cargo test -p desktop-session-mgr --lib`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libui::element::{
    Element, Insets, TextSize, bold, center_v, column, ink, padding, row, scaled, sized, text,
    with_spacing,
};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};
use libui::widget::{POPUP_BORDER, TextFieldState, WidgetState, popup_frame, text_field};

/// The greeter window's size. Fixed rather than screen-relative: only its position follows the
/// screen, centred on the size `/dev/draw/screen` reports since Phase 5 Part E (see `_start`).
///
/// **The height is the card's own, measured** (desktop refresh, Part D):
/// `the_card_is_exactly_the_window_it_is_drawn_in` fails if the view stops being this tall, which
/// is what keeps a restyle from leaving a band of empty card under the fields. It was 420×200,
/// and the content reached barely past the halfway line.
///
/// **The width is a choice and not a measurement**, because the fields flex: the card is as wide
/// as its content only if nothing in it stretches. This is what leaves the fields a sensible box
/// to type in — the test asserts the label column and both fields fit inside it.
pub const GREETER_W: u32 = 340;
/// See [`GREETER_W`].
pub const GREETER_H: u32 = 141;
/// Bytes per row. `WIDTH * 4` exactly: nothing here needs the padded pitch the reference UI
/// uses to catch stride bugs, and an unpadded one keeps the buffer copy a memcpy.
pub const GREETER_PITCH: usize = (GREETER_W as usize) * 4;
/// The space between the card's edge and what is in it.
const CARD_PAD: u32 = 18;
/// Between the heading, the refusal's line and each field.
const ROW_GAP: u32 = 8;
/// A field's height — the chooser's, so a box to type in is one size across the system.
const FIELD_H: u32 = 24;
/// The label column beside each field. Fixed rather than measured: two labels that did not share
/// an edge would read as two unrelated rows.
const LABEL_W: u32 = 74;
/// The refusal's line, present whether or not there is one to say.
const REFUSAL_H: u32 = 16;

/// The theme the greeter draws with, and the one whose font paths it loads.
///
/// **The built-in theme, and the greeter is the one surface that cannot have another.** A theme
/// lives in a user's home (M11 Part C) and this runs *before* there is a user — asking who they
/// are is what this window is for.
///
/// **One function rather than two `Theme::default()` calls**: the binary resolves the font from a
/// theme and the view is painted with one, and those being the same theme is what keeps the face
/// on screen the face that was measured.
pub fn greeter_theme() -> Theme {
    Theme::default()
}

/// Which field the keyboard is going to.
#[derive(Clone, Copy, PartialEq, Eq)]
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

/// `EV_KEY` codes the greeter acts on itself. The fields claim everything else through
/// [`TextFieldState::apply`], which declines exactly these three so they can reach here — the
/// reason `Element::on_key` returns an `Option` at all.
pub const KEY_TAB: u16 = 15;
/// See [`KEY_TAB`].
pub const KEY_ENTER: u16 = 28;

impl Greeter {
    /// An empty greeter, caret in the username field.
    pub fn new() -> Self {
        Self {
            user: TextFieldState::new(),
            password: TextFieldState::new(),
            focus: Focus::User,
            denied: false,
        }
    }

    /// The element tree for the current state.
    ///
    /// Rebuilt per frame, which is the toolkit's model: `view(&state) -> Element`.
    ///
    /// **The theme comes from the caller**, because the caller is what paints this tree — with
    /// [`greeter_theme`], for the reason that function gives. One frame built from one theme and
    /// painted with another is the mistake one type makes easy and the old two-type split made
    /// unwriteable (PR #262 review, optional 5).
    ///
    /// **The design never drew a greeter**, so this reads its language off the surfaces it did
    /// draw (desktop refresh, Part D): a card with a one-pixel edge rounded like every other
    /// floating surface, a heading in the bold face, labels in the dim ink a second-read line
    /// gets everywhere else, and fields with an edge of their own.
    pub fn view(&self, theme: &Theme) -> Element<()> {
        let active = |f: Focus| WidgetState { active: self.focus == f, ..Default::default() };
        let field = |f: &TextFieldState, masked, which| {
            sized(Size::new(0, FIELD_H), text_field(f, masked, active(which), theme))
        };
        let labelled = |label: &str, f: Element<()>| {
            row(alloc::vec![
                sized(
                    Size::new(LABEL_W, FIELD_H),
                    center_v(ink(theme.foreground_dim, text(label))),
                ),
                f.flex(1),
            ])
        };
        let mut rows = alloc::vec::Vec::with_capacity(4);
        rows.push(bold(scaled(TextSize::Large, text("nitrox"))));
        // **The refusal's line is always here, and empty until there is one.** It used to be
        // pushed in when it happened, which moved both fields down the card as it appeared and
        // back up on the next keystroke — in a window whose size is fixed, that is the content
        // jumping under the caret. Said above the fields and cleared on the next keystroke: the
        // serial column prints `login incorrect` for the same reason a window does, that a
        // refusal a user cannot see is a login that appears to have done nothing.
        let refusal = if self.denied { "login incorrect" } else { "" };
        rows.push(sized(
            Size::new(0, REFUSAL_H),
            center_v(ink(theme.deny, scaled(TextSize::Small, text(refusal)))),
        ));
        rows.push(labelled("username", field(&self.user, false, Focus::User)));
        rows.push(labelled("password", field(&self.password, true, Focus::Password)));
        popup_frame(padding(Insets::all(CARD_PAD), with_spacing(column(rows), ROW_GAP)), theme)
    }

    /// The field the caret is in.
    fn active_field(&mut self) -> &mut TextFieldState {
        match self.focus {
            Focus::User => &mut self.user,
            Focus::Password => &mut self.password,
        }
    }

    /// Apply a key. `true` if anything changed and the greeter must be redrawn.
    ///
    /// **Tab and Enter are handled here, not by the field**, which is the split
    /// `Element::on_key`'s `Option` return exists for: a field that swallowed Tab could never
    /// be left, and one that swallowed Enter could never submit.
    pub fn key(&mut self, keycode: u16, modifiers: u16) -> bool {
        match keycode {
            KEY_TAB => {
                self.focus = match self.focus {
                    Focus::User => Focus::Password,
                    Focus::Password => Focus::User,
                };
                true
            }
            _ => {
                // Any edit clears a previous refusal: a "login incorrect" that outlives the
                // typing that answers it reads as a second failure.
                let changed = self.active_field().apply(keycode, modifiers);
                if changed && self.denied {
                    self.denied = false;
                }
                changed
            }
        }
    }

    /// Clear both fields and put the caret back — after a session ends, and after a refusal.
    ///
    /// **The password leaves the screen the moment it has been read**, whichever way the
    /// attempt went. A greeter outlives every session it starts, so one left in the field
    /// would sit behind whatever the session drew.
    ///
    /// **Not a scrub.** `String::clear` sets the length to zero and leaves the bytes in the
    /// allocation, so this is a claim about what is displayed and what a later attempt can
    /// read back — not about this process's memory. The caller's stack copy is
    /// volatile-zeroed after the session ends, which is the same distinction (PR #236 review,
    /// finding 8).
    pub fn reset(&mut self) {
        self.user.clear();
        self.password.clear();
        self.focus = Focus::User;
    }

    /// Render the current state into a fresh framebuffer.
    pub fn render(&self, font: &Font) -> MemFramebuffer {
        let geometry =
            Geometry::with_pitch(GREETER_W, GREETER_H, GREETER_PITCH, PixelFormat::XRGB8888)
                .expect("the greeter pitch is wide enough for a row");
        let mut fb = MemFramebuffer::new(geometry);
        // Built once and used for both the tree and the paint, so one frame is never two themes.
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

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");
    const BOLD: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans-Bold.ttf");

    /// The faces the greeter draws with: the built-in UI face and its bold, as `load_ui`
    /// attaches them in the guest.
    fn faces() -> Font {
        Font::from_bytes(DEJAVU.to_vec())
            .expect("the vendored face parses")
            .with_bold(Font::from_bytes(BOLD.to_vec()).expect("its bold parses"))
    }

    /// A refusal is drawn, and drawn in `deny` — not in the ink everything else uses.
    ///
    /// **Painted, because this is a claim about pixels**: the view carries the word either way,
    /// so a tree inspection would pass against a refusal painted in the body colour, and against
    /// one painted in the card's own white. The control is the resting greeter below it, which
    /// must put down none of that colour at all — the line is there and empty.
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
                    // **A red cast rather than the exact colour**: at the small step a refusal
                    // is set in, antialiasing leaves only a handful of pixels at `deny` itself
                    // — fifteen, measured — and every other colour on this card is a grey, a
                    // white or the teal accent, none of which lean red.
                    if let Some(c) = fb.get_pixel(x, y) {
                        if c.r > c.g.saturating_add(20) && c.r > c.b.saturating_add(20) {
                            n += 1;
                        }
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

    /// The card is exactly the window it is drawn in, and stays that height when refused.
    ///
    /// **This is the pair `check-login` writes down** (desktop refresh, Part D.4): the gate has
    /// its own copy of `GREETER_W`×`GREETER_H`, as every chrome metric in that file does (M11
    /// decision 2), and this is what makes the copy safe to keep — the two cannot drift without
    /// a host test failing first, which costs a second rather than a boot.
    #[test]
    fn the_card_is_exactly_the_window_it_is_drawn_in() {
        let f = faces();
        let t = greeter_theme();
        let m = FontMetrics::new(&f, t.font_px);
        let measure = |g: &Greeter| {
            libui::layout::measure(
                &g.view(&t),
                libui::layout::Constraints::loose(Size::new(4000, 4000)),
                &m,
            )
        };
        let mut g = Greeter::new();
        assert_eq!(measure(&g).h, GREETER_H, "the window is the height of what is in it");
        // **The refusal does not resize the card**, which is why its line is always there: the
        // window's size is fixed at creation, so a taller view would be clipped and a shorter
        // one would leave a band of ground — and either way the fields would move under the
        // caret as a message came and went.
        g.denied = true;
        assert_eq!(measure(&g).h, GREETER_H, "a refusal changes nothing about the card's height");
        // And the width holds the label column and a field wide enough to be one.
        let content = GREETER_W - 2 * (CARD_PAD + POPUP_BORDER);
        assert!(content > LABEL_W, "the label column alone does not fill the card");
        assert!(content - LABEL_W >= 200, "a field {} wide is too narrow", content - LABEL_W);
        assert!(measure(&g).w <= GREETER_W, "the card's own width fits the window");
    }
}
