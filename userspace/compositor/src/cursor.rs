//! The pointer's sprite, the move outline, and key repeat.

use libdraw::format::Rgb;
use libdraw::framebuffer::Framebuffer;
use libdraw::geom::{Point, Rect};
use libdraw::theme::Theme;

/// The pointer sprite: a plain arrow, `CURSOR_W × CURSOR_H`.
///
/// `#` is the body, `.` the outline, ` ` transparent. One fixed shape, not a protocol.
/// `tools/build-cache/cursor-sprite.py` renders edits to it magnified.
pub(crate) const CURSOR: [&str; CURSOR_H as usize] = [
    ".",
    "..",
    ".#.",
    ".##.",
    ".###.",
    ".####.",
    ".#####.",
    ".######.",
    ".#######.",
    ".########.",
    ".#####....",
    ".#####.",
    ".##..##.",
    ".#...##..",
    "... ..##.",
    "     .##.",
    "     ....",
];

/// Cursor sprite width.
pub const CURSOR_W: u32 = 12;
/// Cursor sprite height.
pub const CURSOR_H: u32 = 17;

/// The cursor's fill colour (`#` in the sprite).
///
/// From the shared theme in `libdraw`; compiled in, as the compositor never sees the theme record.
pub const CURSOR_BODY: Rgb = Theme::light().cursor_body;
/// The cursor's outline colour (`.` in the sprite), so it stays visible against white.
pub const CURSOR_OUTLINE: Rgb = Theme::light().cursor_outline;

/// The rectangle a cursor at `at` occupies; the hotspot is the top-left.
pub fn cursor_rect(at: Point) -> Rect {
    Rect::new(at.x, at.y, CURSOR_W, CURSOR_H)
}

/// Draw the pointer sprite at `at`, clipped to `clip`, after the window stack is composed.
pub fn draw_cursor<F: Framebuffer + ?Sized>(fb: &mut F, at: Point, clip: Rect) {
    for (row, line) in CURSOR.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let colour = match ch {
                '#' => CURSOR_BODY,
                '.' => CURSOR_OUTLINE,
                _ => continue,
            };
            let x = at.x.saturating_add(col as i32);
            let y = at.y.saturating_add(row as i32);
            // Clipped by the rectangle and the buffer: at the screen's edge it must not wrap.
            if x >= 0 && y >= 0 && clip.contains(x, y) {
                fb.put_pixel(x as u32, y as u32, colour);
            }
        }
    }
}

/// The interactive-resize outline's thickness, in pixels.
pub const OUTLINE_W: u32 = 2;

/// The outline's colour — bright enough to read over any client's pixels.
pub const OUTLINE_COLOUR: Rgb = Theme::light().outline;

/// The four edge strips of `rect`, which is what an outline occupies; strips because it is
/// repainted per pointer motion and a full recompose starves input.
pub fn outline_edges(rect: Rect) -> [Rect; 4] {
    let t = OUTLINE_W.min(rect.size.h.max(1));
    let side = OUTLINE_W.min(rect.size.w.max(1));
    [
        Rect::new(rect.origin.x, rect.origin.y, rect.size.w, t),
        Rect::new(rect.origin.x, rect.bottom() as i32 - t as i32, rect.size.w, t),
        Rect::new(rect.origin.x, rect.origin.y, side, rect.size.h),
        Rect::new(rect.right() as i32 - side as i32, rect.origin.y, side, rect.size.h),
    ]
}

/// Draw the interactive-resize outline at `rect`, clipped to `clip`.
pub fn draw_outline<F: Framebuffer + ?Sized>(fb: &mut F, rect: Rect, clip: Rect) {
    for strip in outline_edges(rect) {
        let Some(r) = strip.intersect(&clip) else { continue };
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                if x >= 0 && y >= 0 {
                    fb.put_pixel(x as u32, y as u32, OUTLINE_COLOUR);
                }
            }
        }
    }
}

/// How long a key must be held before it starts repeating, in nanoseconds.
pub const REPEAT_DELAY_NS: u64 = 400_000_000;

/// How long between repeats once they start, in nanoseconds.
pub const REPEAT_INTERVAL_NS: u64 = 40_000_000;

/// A key held down, and when it next repeats; compositor-side so a repeat stops when focus moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Repeat {
    /// The keycode being repeated.
    pub keycode: u16,
    /// Modifiers held when it went down, frozen so releasing shift mid-repeat does not turn `A`
    /// into `a`.
    pub modifiers: u16,
    /// The locks on when it went down, frozen for the same reason.
    pub locks: u16,
    /// The window it is going to.
    pub window: u32,
    /// When the next repeat is due.
    pub next_at: u64,
}

impl Repeat {
    /// Start repeating `keycode` for `window`, first repeat one delay from `now`.
    pub fn armed(keycode: u16, modifiers: u16, locks: u16, window: u32, now: u64) -> Self {
        Self { keycode, modifiers, locks, window, next_at: now.saturating_add(REPEAT_DELAY_NS) }
    }

    /// The `KeyEvent` each repeat sends: the key, `KEY_REPEAT`, and its modifiers and locks.
    pub fn event(&self) -> librsproto::surface::KeyEvent {
        librsproto::surface::KeyEvent::new(self.window, self.keycode, librsproto::surface::KEY_REPEAT, self.modifiers)
            .with_locks(self.locks)
    }

    /// Whether a repeat is due at `now`; advances to the next one if so.
    pub fn due(&mut self, now: u64) -> bool {
        if now < self.next_at {
            return false;
        }
        self.next_at = self.next_at.saturating_add(REPEAT_INTERVAL_NS);
        // A long stall must not fire a burst catching up on repeats.
        if self.next_at <= now {
            self.next_at = now.saturating_add(REPEAT_INTERVAL_NS);
        }
        true
    }

    /// The repeat state after a key transition — arm, disarm, or leave alone.
    ///
    /// A modifier or lock key never arms and never disturbs; only the repeating key's own release
    /// disarms; a press with no focus candidate disarms.
    pub fn after_key(
        current: Option<Repeat>,
        keycode: u16,
        pressed: bool,
        modifiers: u16,
        locks: u16,
        focus: Option<u32>,
        now: u64,
    ) -> Option<Repeat> {
        if libinput::is_modifier(keycode) || libinput::is_lock_key(keycode) {
            return current;
        }
        match (pressed, focus) {
            (true, Some(window)) => Some(Repeat::armed(keycode, modifiers, locks, window, now)),
            (true, None) => None,
            (false, _) if current.is_some_and(|r| r.keycode == keycode) => None,
            (false, _) => current,
        }
    }
}
