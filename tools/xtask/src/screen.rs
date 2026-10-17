//! The screen a gate boots: its size, where things are on it, and a screendump read as text.

use crate::*;

/// The size of the screen QEMU gives the guest. **QEMU's EDID prefers the mode**: `-vga none
/// -device VGA,xres=…,yres=…` makes OVMF add the mode and boot into it. **A width that is not a
/// multiple of 8 shears**: QEMU's standard VGA rounds `XRES` down while OVMF's GOP reports the
/// width asked for, which is why the laptop's 1366 is refused and the gates boot
/// [`DisplaySize::GATE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplaySize {
    pub(crate) w: u32,
    pub(crate) h: u32,
}

impl DisplaySize {
    /// The laptop's screen as near as QEMU shows it (768 rows, a 170×48 console).
    pub(crate) const GATE: DisplaySize = DisplaySize { w: 1360, h: 768 };

    /// The height of the shell's bars: `desktop_shell::BAR_H`, **written down a second time on
    /// purpose**, so a gate cannot agree with a shell that stopped drawing where it says.
    pub(crate) const BAR_H: i32 = 30;
    /// Where the bottom bar's first task button starts, how wide one is, and the step to the next
    /// (`desktop_shell::panel::TASKS_X`, `TASK_W`, `TASK_PITCH`); the 48 pixels are the
    /// show-desktop button and its rule.
    const TASKS_X: i32 = 48;
    const TASK_W: i32 = 186;
    const TASK_PITCH: i32 = 190;
    const SHOW_DESKTOP_X: i32 = 20;
    /// The desktop's name at the switcher's right-hand end, from the screen's right edge: inside
    /// the name for any name of two characters or more (pinned by `panel`'s own test).
    const NAME_FROM_RIGHT: i32 = 30;
    /// The overview card's width as a fraction of the screen (`desktop_shell::CARD_W_NUM` over
    /// `CARD_W_DEN`): the gate's own copy, kept honest by `the_gates_card_metrics_are_the_shells`.
    pub(crate) const CARD_W_NUM: i32 = 330;
    pub(crate) const CARD_W_DEN: i32 = 1440;
    /// A card's border, its gap to the next, and the gap and height of its caption.
    pub(crate) const CARD_BORDER: i32 = 2;
    pub(crate) const CARD_GAP: i32 = 24;
    pub(crate) const CARD_CAPTION_GAP: i32 = 9;
    pub(crate) const CARD_CAPTION_H: i32 = 18;
    pub(crate) const CARD_SIDE_PAD: i32 = 40;

    /// Parse `WxH`, refusing a size QEMU cannot show or a guest cannot use.
    pub(crate) fn parse(text: &str) -> R<DisplaySize> {
        let (w, h) = text
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .ok_or_else(|| format!("`{text}` is not a size — write WxH, e.g. 1360x768"))?;
        if w % 8 != 0 {
            return Err(format!(
                "a {w}-pixel width is not a multiple of 8: QEMU's VGA would scan out {} while the \
                 guest draws {w}, and every row would shear — the reason the gates boot 1360, not \
                 the laptop's 1366",
                w & !7
            )
            .into());
        }
        if !(640..=4096).contains(&w) || !(480..=2160).contains(&h) {
            return Err(format!("{w}x{h} is outside 640x480 to 4096x2160").into());
        }
        Ok(DisplaySize { w, h })
    }

    pub(crate) fn corner(self) -> (i32, i32) {
        (self.w as i32 - 1, self.h as i32 - 1)
    }

    pub(crate) fn centre(self) -> (i32, i32) {
        (self.w as i32 / 2, self.h as i32 / 2)
    }

    /// The y the shell places its bottom bar at, and the `bottom bar placed at 0,<y>` it logs.
    pub(crate) fn bottom_bar_y(self) -> i32 {
        self.h as i32 - Self::BAR_H
    }

    pub(crate) fn bottom_bar_click_y(self) -> i32 {
        self.bottom_bar_y() + Self::BAR_H / 2
    }

    /// The desktop's name on the bottom bar, which opens the overview.
    pub(crate) fn desktop_name_click(self) -> (i32, i32) {
        (self.w as i32 - Self::NAME_FROM_RIGHT, self.bottom_bar_click_y())
    }

    pub(crate) fn task_click(self, slot: usize) -> (i32, i32) {
        (Self::TASKS_X + Self::TASK_PITCH * slot as i32 + Self::TASK_W / 2, self.bottom_bar_click_y())
    }

    pub(crate) fn show_desktop_click(self) -> (i32, i32) {
        (Self::SHOW_DESKTOP_X, self.bottom_bar_click_y())
    }

    pub(crate) fn card_w(self) -> i32 {
        (self.w as i32 * Self::CARD_W_NUM / Self::CARD_W_DEN).max(Self::CARD_BORDER * 2 + 2)
    }

    fn scaled_to_card(self, n: i32) -> i32 {
        n * (self.card_w() - Self::CARD_BORDER * 2) / self.w.max(1) as i32
    }

    /// A card's miniature box, border included — the strip is the top bar at the card's scale.
    fn card_h(self) -> i32 {
        self.scaled_to_card(Self::BAR_H) + self.scaled_to_card(self.h as i32 - Self::BAR_H) + Self::CARD_BORDER * 2
    }

    fn card_total_h(self) -> i32 {
        self.card_h() + Self::CARD_CAPTION_GAP + Self::CARD_CAPTION_H
    }

    pub(crate) fn card_cols(self) -> i32 {
        let avail = self.w as i32 - Self::CARD_SIDE_PAD * 2;
        ((avail + Self::CARD_GAP) / (self.card_w() + Self::CARD_GAP)).max(1)
    }

    /// The top-left of card `i` of `n` in the overview (the shell's `card_rect`, recomputed):
    /// centred both ways, between the two bars.
    fn card_origin(self, i: i32, n: i32) -> (i32, i32) {
        let (cols, cw) = (self.card_cols(), self.card_w());
        let (row, col) = (i / cols, i % cols);
        let rows = n.div_euclid(cols) + i32::from(n.rem_euclid(cols) != 0);
        let in_row = (n - row * cols).min(cols);
        let across = in_row * cw + (in_row - 1).max(0) * Self::CARD_GAP;
        let x = (self.w as i32 - across) / 2 + col * (cw + Self::CARD_GAP);
        let total = self.card_total_h();
        let down = rows * total + (rows - 1).max(0) * Self::CARD_GAP;
        let avail = self.h as i32 - Self::BAR_H * 2;
        let y = Self::BAR_H + (avail - down) / 2 + row * (total + Self::CARD_GAP);
        (x, y)
    }

    /// A point inside card `i` of `n`, clear of its border and of any window: the bottom-left of
    /// its box.
    pub(crate) fn card_click(self, i: i32, n: i32) -> (i32, i32) {
        let (x, y) = self.card_origin(i, n);
        (x + Self::CARD_BORDER + 6, y + self.card_h() - Self::CARD_BORDER - 6)
    }

    /// The middle of the box a window at `(wx, wy)` sized `(ww, wh)` is drawn in, inside card `i`
    /// of `n` (the shell's `window_box`, recomputed), where a press that picks it up lands.
    pub(crate) fn window_in_card(self, i: i32, n: i32, w: (i32, i32, u32, u32)) -> (i32, i32) {
        let (cx, cy) = self.card_origin(i, n);
        let sx = self.scaled_to_card(w.0.max(0));
        let sy = self.scaled_to_card(Self::BAR_H) + self.scaled_to_card(w.1.max(Self::BAR_H) - Self::BAR_H);
        let sw = self.scaled_to_card(w.2 as i32).max(2);
        let sh = self.scaled_to_card(w.3 as i32).max(2);
        (cx + Self::CARD_BORDER + sx + sw / 2, cy + Self::CARD_BORDER + sy + sh / 2)
    }

    /// How many `(100, 100)` motions pin the pointer into the bottom-right corner from anywhere:
    /// the longer side over-driven, never fewer than twenty. Twenty (2000 px) does not cross a
    /// 2560-wide screen.
    pub(crate) fn pin_motions(self) -> usize {
        (self.w.max(self.h) as usize / 100 + 2).max(20)
    }
}

impl std::fmt::Display for DisplaySize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.w, self.h)
    }
}

/// One screendump, read as the kernel console would have drawn it.
pub(crate) struct Frame {
    /// Per row: its text when every cell is one glyph in ink on paper, trailing blanks trimmed.
    pub(crate) rows: Vec<Option<String>>,
    /// Cells anywhere that read as a glyph other than a blank: a stray paint can cover part of a
    /// row.
    pub(crate) text_cells: usize,
    sample: String,
}

impl Frame {
    /// Read a screendump. A screen the console has no geometry for reads as no rows at all.
    pub(crate) fn read(w: u32, h: u32, rgb: &[u8]) -> Frame {
        let (w, h) = (w as usize, h as usize);
        let mut frame = Frame { rows: Vec::new(), text_cells: 0, sample: String::new() };
        let Some(g) = text::Geometry::for_screen(w, h) else { return frame };
        if rgb.len() < w * h * 3 {
            return frame;
        }
        let (cw, ch) = (g.cell_w(), g.cell_h());
        for row in 0..g.rows {
            let mut line = Some(String::new());
            for col in 0..g.cols {
                let mut bitmap = [0u8; glyphs::GLYPH_H];
                let mut clean = true;
                'cell: for y in 0..ch {
                    for x in 0..cw {
                        let i = ((row * ch + y) * w + col * cw + x) * 3;
                        let px = [rgb[i], rgb[i + 1], rgb[i + 2]];
                        // At scale > 1 a glyph pixel is a block, read as one only if the whole
                        // block is one colour: OR-ing it would pass a quarter-painted cell.
                        let b = ((row * ch + y / g.scale * g.scale) * w + col * cw + x / g.scale * g.scale) * 3;
                        if px != [rgb[b], rgb[b + 1], rgb[b + 2]] {
                            clean = false;
                            break 'cell;
                        }
                        if px == text::INK {
                            bitmap[y / g.scale] |= 0x80 >> (x / g.scale);
                        } else if px != text::PAPER {
                            clean = false;
                            break 'cell;
                        }
                    }
                }
                let c = if clean { glyphs::identify(&bitmap).and_then(glyphs::char_of) } else { None };
                match (c, line.as_mut()) {
                    (Some(c), l) => {
                        if c != ' ' {
                            frame.text_cells += 1;
                            if frame.sample.chars().count() < 40 {
                                frame.sample.push(c);
                            }
                        }
                        if let Some(l) = l {
                            l.push(c);
                        }
                    }
                    (None, _) => line = None,
                }
            }
            frame.rows.push(line.map(|l| l.trim_end().to_string()));
        }
        frame
    }

    pub(crate) fn is_console(&self) -> bool {
        !self.rows.is_empty() && self.rows.iter().all(Option::is_some)
    }

    pub(crate) fn has_all(&self, wanted: &[&str]) -> bool {
        let lines = self.lines();
        wanted.iter().all(|w| lines.contains(w))
    }

    pub(crate) fn lines(&self) -> Vec<&str> {
        self.rows.iter().flatten().map(String::as_str).collect()
    }

    pub(crate) fn cells_sample(&self) -> &str {
        &self.sample
    }
}

/// How many screendumps the display gate takes waiting for two in a row to match, and the wait
/// between them (~3 s, against a repaint of tens of ms).
const SCREEN_SETTLE_TRIES: usize = 12;

const SCREEN_SETTLE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Screendump until two consecutive captures are identical. **Two identical dumps, not a sleep**: a
/// fixed delay is a guess that goes stale as the scene grows, and a stable but wrong screen still
/// fails the comparison afterwards.
pub(crate) fn settle_and_capture(qmp: &mut Qmp, shot: &Path) -> R<Vec<u8>> {
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..SCREEN_SETTLE_TRIES {
        qmp.screendump(shot)?;
        let bytes = fs::read(shot).map_err(|e| format!("read screendump {}: {e}", shot.display()))?;
        if prev.as_deref() == Some(bytes.as_slice()) {
            return Ok(bytes);
        }
        prev = Some(bytes);
        std::thread::sleep(SCREEN_SETTLE_WAIT);
    }
    Err(format!(
        "the guest's screen never stopped changing over {SCREEN_SETTLE_TRIES} captures \
         {SCREEN_SETTLE_WAIT:?} apart — a compositor repainting continuously is itself the bug"
    )
    .into())
}

/// Parse a binary PPM (P6) into `(width, height, rgb_bytes)`: QEMU writes it, as does
/// `libdraw::ppm`.
pub(crate) fn parse_ppm(data: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0usize;
    while fields.len() < 4 && i < data.len() {
        while i < data.len() && data[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < data.len() && data[i] == b'#' {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < data.len() && !data[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..i]).into_owned());
    }
    if fields.len() != 4 || fields[0] != "P6" {
        return Err(format!("not a binary PPM (fields: {fields:?})").into());
    }
    let w: u32 = fields[1].parse().map_err(|_| "bad PPM width")?;
    let h: u32 = fields[2].parse().map_err(|_| "bad PPM height")?;
    if fields[3] != "255" {
        return Err(format!("unsupported PPM maxval {}", fields[3]).into());
    }
    i += 1; // the single whitespace byte after maxval
    let need = w as usize * h as usize * 3;
    if data.len() < i + need {
        return Err(format!("PPM body short: {} < {need}", data.len() - i).into());
    }
    Ok((w, h, data[i..i + need].to_vec()))
}
