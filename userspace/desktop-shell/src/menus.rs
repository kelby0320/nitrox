use super::*;

/// The bottom bar's tree as things are now.
#[allow(clippy::too_many_arguments, reason = "the bar's state, theme and screen, each a separate borrow")]
pub(super) fn bottom_view(
    entries: &[WinEntry],
    desktops: &[Desktop],
    current: u32,
    shown: bool,
    hovered: Option<u64>,
    theme: &Theme,
    font: &Font,
    screen: Screen,
) -> Element<BottomMsg> {
    let cap = task_capacity(desktops, entries, current, theme, font, screen);
    let visible = visible_entries(entries, current, cap);
    // **The title, or the id when there is none** — a window that has not named itself still
    // needs a button to click. The serial log's markers are not drawn: the dot says it.
    let titles: alloc::vec::Vec<alloc::string::String> = visible.iter().map(|e| entry_title(e)).collect();
    let tasks: alloc::vec::Vec<panel::Task<'_>> = visible
        .iter()
        .zip(titles.iter())
        .map(|(e, t)| panel::Task { id: e.id, title: t.as_str(), focused: e.focused, minimized: e.minimized })
        .collect();
    let (occupied, at, label) = desktop_facts(desktops, entries, current);
    let d = panel::Desktops { occupied: &occupied, current: at, label: &label };
    panel::bottom_bar(&tasks, shown, &d, hovered, theme)
}

/// Draw the bottom bar as things are now, if anything it shows has changed.
#[allow(clippy::too_many_arguments, reason = "the bar's state and how to draw it, each a separate borrow")]
pub(super) fn present_bottom(
    session: &mut Session<ChannelTransport>,
    bar: Option<&mut Child>,
    entries: &[WinEntry],
    desktops: &[Desktop],
    current: u32,
    shown: &Option<ShownDesktop>,
    theme: &Theme,
    panel_theme: &Theme,
    font: &Font,
    screen: Screen,
) {
    let Some(bar) = bar else { return };
    let lit = shown.as_ref().is_some_and(|s| s.desktop == current);
    let view = bottom_view(entries, desktops, current, lit, bar.hovered_key(), theme, font, screen);
    if !bar.present(session, &view, font, panel_theme) {
        kprint(b"desktop-shell: bottom bar Commit failed\n");
    }
}

/// The program a place opens in — the folder's counterpart of [`OPENER`].
pub(super) const FOLDER_OPENER: &str = "nxfiles";

/// The top bar's two menus: which one is open, the popup showing it, and what the Applications
/// menu is filtered by (desktop refresh, Part C).
///
/// **One popup at a time, whichever menu it is.** `MenuState` moves between the two with Left and
/// Right; [`sync`](Self::sync) is what makes the popup follow, closing one menu's window and
/// opening the other's. The popup is a `Child`, like every application's menus, so it keeps its
/// own retained tree and router — the two things this shell used to keep beside the modal by hand.
pub(super) struct BarMenus<'a> {
    /// Which menu is open and which row the keyboard is on.
    pub(super) state: MenuState,
    /// The popup, and which menu it is showing.
    pub(super) win: Option<(usize, Child)>,
    /// What the Applications menu is filtered by. Cleared when it closes: a launcher that reopened
    /// still filtered by the last thing launched would be showing a stale answer to a question
    /// nobody asked.
    pub(super) query: TextFieldState,
    /// The desktop entries, in the order the rows' messages index.
    pub(super) programs: &'a [Application],
    /// The places, in the order the rows' messages index.
    pub(super) places: &'a [libfs::Place],
    /// The home an application sees, which the places are written from.
    pub(super) home: &'a str,
    /// Where each menu hangs, by index — see [`menu_anchors`].
    pub(super) anchors: [(i32, i32); 3],
}

impl BarMenus<'_> {
    /// The two menus as they are now.
    pub(super) fn table(&self, theme: &Theme) -> alloc::vec::Vec<libui::menu::Menu<MenuMsg>> {
        panel::menus(self.programs, self.query.text(), self.places, self.home, theme)
    }

    /// The popup's window, if one is up.
    pub(super) fn id(&self) -> Option<u32> {
        self.win.as_ref().map(|(_, c)| c.id())
    }

    /// What the log calls the menu that is open — or was, for a line about closing it.
    pub(super) fn what(&self) -> &'static str {
        match self.win.as_ref().map(|(w, _)| *w).or(self.state.open()) {
            Some(panel::PLACES) => "places menu",
            Some(panel::POWER) => "power menu",
            _ => "applications menu",
        }
    }

    /// Close whatever is open, choosing nothing.
    pub(super) fn close(&mut self, session: &mut Session<ChannelTransport>) {
        self.state.close();
        self.drop_popup(session);
    }

    /// Destroy the popup and say so, leaving `state` alone.
    fn drop_popup(&mut self, session: &mut Session<ChannelTransport>) {
        let what = self.what();
        if let Some((which, c)) = self.win.take() {
            let id = c.id();
            c.close(session);
            if which == panel::APPS {
                self.query.clear();
            }
            // **Named, because two menus share this path.** These gates read the serial log as
            // the shell's only externally visible output, and a Places dismissal reading as the
            // Applications menu's is the line a later gate would assert the wrong thing about
            // (PR #243 review, optional 8).
            Line::new().s(b"desktop-shell: ").s(what.as_bytes()).s(b" closed, window ").u(id as u64).end();
        }
    }

    /// Make the popup show what `state` says is open: open it, close it, switch it to the other
    /// menu, or redraw it. Returns the id of a popup this opened, which the caller makes sticky.
    pub(super) fn sync(
        &mut self,
        session: &mut Session<ChannelTransport>,
        parent: u32,
        theme: &Theme,
        font: &Font,
    ) -> Option<u32> {
        let want = self.state.open();
        if want == self.win.as_ref().map(|(w, _)| *w) {
            self.redraw(session, theme, font);
            return None;
        }
        self.drop_popup(session);
        let which = want?;
        let table = self.table(theme);
        let view = panel::menu_view(which, &table, &self.state, None, &self.query, theme);
        let Some(mut c) =
            Child::open(session, Role::Popup { parent }, self.anchors[which], &view, font, theme, BUFFERS)
        else {
            Line::new().s(b"desktop-shell: ").s(self.what().as_bytes()).s(b" CreateWindow FAILED").end();
            self.state.close();
            return None;
        };
        // **Drawn before it is announced**, so a line saying it is open is a line about a window
        // that has committed a frame — an uncommitted popup is invisible and still takes the
        // keyboard, which is the worst of both.
        if !c.present(session, &view, font, theme) {
            kprint(b"desktop-shell: a menu could not draw its first frame\n");
            c.close(session);
            self.state.close();
            return None;
        }
        let id = c.id();
        let mut l = Line::new();
        l.s(b"desktop-shell: ");
        if which == panel::APPS {
            l.s(b"applications menu open, window ").u(id as u64).s(b" listing ").u(self.programs.len() as u64);
        } else {
            // **Named by `what`**, which knows all three: a two-way branch here called the power
            // menu the places menu (administration Part F.2).
            l.s(self.what().as_bytes()).s(b" open, window ").u(id as u64);
        }
        l.end();
        self.win = Some((which, c));
        Some(id)
    }

    /// Redraw the popup with the lit row and the filter as they are now — **at a new size** when
    /// the filter has changed how many rows there are, since a menu is as tall as what is in it.
    pub(super) fn redraw(&mut self, session: &mut Session<ChannelTransport>, theme: &Theme, font: &Font) {
        let table = panel::menus(self.programs, self.query.text(), self.places, self.home, theme);
        let Some((which, c)) = self.win.as_mut() else { return };
        let view = panel::menu_view(*which, &table, &self.state, c.hovered_key(), &self.query, theme);
        let size = libui::layout::measure(
            &view,
            libui::layout::Constraints::loose(libdraw::geom::Size::new(u32::MAX / 4, u32::MAX / 4)),
            &FontMetrics::new(font, theme.font_px),
        );
        if size.w > 0 && size.h > 0 && c.resize(size) == Some(false) {
            kprint(b"desktop-shell: a menu could not be resized\n");
        }
        c.present(session, &view, font, theme);
    }

    /// Route a pointer event through the popup, returning the row it chose.
    pub(super) fn route(&mut self, font: &Font, theme: &Theme, event: &libsurface::WindowEvent) -> alloc::vec::Vec<MenuMsg> {
        let table = panel::menus(self.programs, self.query.text(), self.places, self.home, theme);
        let Some((which, c)) = self.win.as_mut() else { return alloc::vec::Vec::new() };
        let view = panel::menu_view(*which, &table, &self.state, c.hovered_key(), &self.query, theme);
        c.route(&view, font, theme, event)
    }
}

/// Where each menu hangs: under its word on the top bar, by [`panel::APPS`], [`panel::PLACES`]
/// and [`panel::POWER`] — the last **right-aligned** under the power button, so it stays on the
/// screen (administration Part F.2).
///
/// **Read off a layout of the bar**, not written down: a word's position is a fact about the bar,
/// and the menu hanging from it is the one thing that has to agree.
pub(super) fn menu_anchors(clock: &str, theme: &Theme, font: &Font, screen: Screen) -> [(i32, i32); 3] {
    let view = panel::top_bar(clock, None, None, theme);
    let metrics = FontMetrics::new(font, theme.font_px);
    let l = layout(&view, Rect::new(0, 0, screen.width, BAR_H), &metrics);
    let fallback = (panel::MENU_INSET_X, BAR_H as i32 + panel::MENU_DROP);
    let at = |key| libui::layout::locate(&view, &l, key).map_or(fallback, panel::menu_anchor);
    let power = libui::layout::locate(&view, &l, panel::POWER_KEY)
        .map_or(fallback, |word| panel::menu_anchor_right(word, popup_width(&panel::power_menu(), theme, &metrics)));
    [at(panel::APPS_KEY), at(panel::PLACES_KEY), power]
}

/// How wide `menu` draws as a popup — what a right-hung menu's anchor needs.
fn popup_width(menu: &libui::menu::Menu<MenuMsg>, theme: &Theme, metrics: &FontMetrics<'_>) -> u32 {
    let view = libui::menu::popup(menu, &MenuState::new(1), panel::ROW_KEY_BASE, None, theme);
    libui::layout::measure(
        &view,
        libui::layout::Constraints::loose(libdraw::geom::Size::new(u32::MAX / 4, u32::MAX / 4)),
        metrics,
    )
    .w
}
