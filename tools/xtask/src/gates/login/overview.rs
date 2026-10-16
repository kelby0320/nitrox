//! `check-login`'s window list, overview, and windows dragged, resized and snapped.

use crate::*;

/// `check-login`'s steps 6b to 6e: the window list, show desktop, the lifecycle rule, the
/// switcher, the overview and `/dev/desktop`.
pub(crate) fn login_window_list_and_overview(
    qmp: &mut Qmp,
    session: &mut Session,
    size: DisplaySize,
    st: &mut LoginState,
) -> R<()> {
    let list_click = st.list_click;
    let term_h = st.term_h;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let wallpaper_id = st.wallpaper_id;
    // 6b. **The window list, and the two things you can do to a window from it** (M8 Part C).
    //
    //     The launched terminal is a `normal` window, so it is listed — and it holds the
    //     keyboard, so the bar shows it focused. Clicking a focused entry puts the window away;
    //     clicking it again brings it back. This is the first thing in the shell that reflects
    //     compositor state continuously rather than at one moment, so the assertions are about
    //     what the list *says*, not only that a click was received.
    //
    //     The bar is the bottom `DisplaySize::BAR_H` rows of the screen, and the first task
    //     button spans x 48 to 234 — so `list_click`, at its middle and half the bar's height, is
    //     inside it at any size.

    // Close the menu first: it is a popup on top, and a press meant for the bar would land in
    // it. **By clicking outside it rather than by pressing Escape** (M11 Part E batch 4) —
    // Escape is covered by the launch step above, and dismissal-on-outside-click is what did not
    // exist: this process never sees a press aimed at another window, so the modal stayed open
    // over whatever was clicked. The compositor's focus event is the one signal that says the
    // person went elsewhere, and it is what closes it now.
    //
    // **Onto bare desktop, which is the case that was broken.** Clicking another *window* raises
    // it, and a raise is a focus change the popup hears about — so the first version of this step
    // clicked into the terminal and passed while the reported bug survived: a press on the
    // desktop or on a panel raises nothing, changes no focus, and left the modal open over it.
    // The compositor sends `Surface::Dismissed` for that press now, which is the half a client
    // cannot see for itself.
    //
    // **The bottom bar's dead space**, between the last task button and the switcher. A panel
    // never takes focus, so a press there raises nothing and produces no focus
    // change *whatever else is on screen* — which is what makes it the honest test. Aiming at
    // bare desktop instead would depend on the terminal not being maximised at this point in the
    // gate, and it is.
    click_at(qmp, session, 600, size.bottom_bar_click_y())?;
    session.expect("desktop-shell: applications menu closed")?;
    // **That the compositor *said* so is checked against the whole transcript below**, not here.
    // The dismissal is logged while the press is being routed and the `press at` line is logged
    // when the routed record is delivered — so the dismissal comes *first*, and `click_at`'s own
    // position assertion has already scanned past it. Same rule this gate applies to every line
    // whose order is an implementation detail rather than a claim.

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: minimized window ")?;
    // **The marker, not just a non-empty list.** The list still holds it — minimizing is not
    // closing, and a taskbar that dropped the entry would leave no way to get the window back —
    // and `_` is how the bar says so. Matching only `window list [` asserted the list existed
    // and nothing about what it shows (PR #242 review, optional 9).
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    // Restored and focused. Focus arrives one iteration after the raise, so this is the second
    // list line the click produces, not the first.
    // **And it is named**, which is the title arm of the list doing something: `nxterm` sets a
    // title, the compositor reports it on `WindowTitle`, and the bar shows it instead of
    // `window 6`. Nothing in the tree sent a title before this part.
    session.expect(":> nxterm")?;

    // 6b2. **Show desktop, and back** (desktop refresh, Part C). The first press puts away every
    //      window this desktop's bar shows and lights the button; the second brings back exactly
    //      what the first put away. The restore set is the shell's, because minimising is a
    //      manager operation — which the plan's first draft put in the compositor (its review,
    //      finding 6). **One window here**, so the counts say the press put something away and
    //      brought it back, and no more: that it brings back *exactly* what it put away — not a
    //      window already minimised, not one past the bar's end — needs more windows than this
    //      step has, and is `desktop_shell::ShownDesktop`'s host tests (PR #314 review, optional
    //      5).
    let show = size.show_desktop_click();
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: showing the desktop, minimised 1 window(s)")?;
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: restored 1 window(s)")?;
    // Focus arrives one iteration after the raise, so this is the second list line, as above.
    session.expect(":> nxterm")?;
    println!("  ok: show-desktop put the window away and brought it back");

    // And the chord, which is the half a taskbar alone does not cover: putting a window away
    // without reaching for its entry. `Super+H`.
    chord(qmp, false, "h")?;
    session.expect("desktop-shell: Super+H minimized window ")?;

    // 6c. **The lifecycle rule, which is Part D's whole claim** (M8 Part D).
    //
    //     Governing decision 3: an **unnamed** empty desktop is removed, a **named** one is
    //     kept, and the list always ends with one empty unnamed desktop to create into. The box
    //     asked for this by *closing* a window — which no gate can do inside a session, since
    //     the only way to close the launched terminal is through its shell and that draws into
    //     the grid, which renders under `test-harness` only (PR #242 review, optional 7). A
    //     desktop also empties when its last window is **moved away**, which is a gesture this
    //     part builds, so the rule is exercised that way instead.
    //
    //     Sequence: name this desktop, move the terminal off it, and show the desktop survived
    //     *because* it is named; then move the terminal back and show the desktop it vacated —
    //     unnamed — is gone.
    // **Restore the terminal first: 6b left it minimized**, and a minimized window is not
    // focused — so `Super+Shift+N`, which moves *the focused window*, would correctly find
    // nothing to move and this block would assert against a gesture that did nothing. Clicking
    // its list entry restores and raises it, which is the gesture Part C added for exactly this.
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    session.expect(":> nxterm")?;

    // Name it. The prompt is the same popup the launcher uses — a `panel` takes no keyboard
    // focus, so the bar itself could never read a typed name.
    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **`Super+A` closes this too, and says which popup it closed.** One window serves both the
    // launcher and the name prompt, so a dismissal that read as the launcher's would send a
    // later gate looking for a line sitting in the transcript under the other name (PR #304
    // review, finding 1). Then reopen it and carry on naming.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: name prompt closed")?;
    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **One character at a time, waiting for each**, the way `type_at_greeter` does. Injection
    // is relative and unacknowledged: a dropped batch ate the `r` and produced a desktop named
    // `wok`, which fails an assertion about naming while saying nothing about naming.
    let mut typed = String::new();
    for c in "work".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        typed.push(c);
        session.expect(&format!("desktop-shell: name so far {typed}"))?;
    }
    press(qmp, "ret")?;
    session.expect("desktop-shell: named this desktop work")?;
    session.expect("desktop-shell: window list on work of 2")?;

    // 6c2. **The switcher** (desktop refresh, Part C): `›` goes to the next desktop, and a cell to
    //      its own. Two desktops here — `work` and the scratch one after it — so two cells.
    //      Aimed from the right edge, where the switcher is anchored; the name beside the arrows
    //      moves them, so `desktop_shell::panel`'s own test pins both aims with `work` and then
    //      `Desktop 2` as the name — the two states this step presses them in.
    const NEXT_FROM_RIGHT: i32 = 67;
    const FIRST_CELL_FROM_RIGHT: i32 = 148;
    let bar_y = size.bottom_bar_click_y();
    click_at(qmp, session, size.w as i32 - NEXT_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;
    click_at(qmp, session, size.w as i32 - FIRST_CELL_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to work")?;
    // **And the terminal has the keyboard again, because it is on the desktop the compositor is
    // showing.** The first runs of this step came back `[N:  nxterm]` — listed, unfocused, and not
    // on screen: the shell had been re-pointed off a removed desktop at step 6a3 without telling
    // the compositor, so "work" was one desktop to the shell and another to the compositor, and
    // nothing showed it until something switched away and back (`sync_current`). The move below
    // depends on this too: it moves the *focused* window.
    session.expect("desktop-shell: window list on work of 2 [")?;
    session.expect(":> nxterm")?;
    println!("  ok: the switcher's arrow and its cell each switched desktops, and back to the window");

    // Move the terminal to the second desktop. `work` is now empty — and **named**, so it
    // stays; the desktop that received the window is no longer the scratch slot, so a new one
    // is appended. Two facts in one line: the bar still says `work`, and there are three.
    chord(qmp, true, "2")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // Follow it, to prove the list filters by desktop rather than merely being emptied.
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // And back to `work`. The desktop just vacated is **unnamed** and empty, so it goes: the
    // count drops from three to two, which is the removal half of the rule.
    //
    // **The bar does not follow the window, and that is the point of reading the count.** A
    // move changes where a window is, not where you are — so the shell stays on the desktop it
    // was on, which has just been emptied. That desktop is now the trailing scratch slot, which
    // the rule keeps, so the reading is "desktop 2 of 2, empty": one desktop fewer than before,
    // and the one that went was the unnamed one.
    chord(qmp, true, "1")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;

    // 6d. **The overview** (M8 Part E): frozen thumbnails of this desktop, a sidebar of the
    //     others, and a window moved by dropping its thumbnail on one.
    //
    //     Opened from the desktop's name at the switcher's end — the indicator's place until the
    //     desktop refresh's Part C, which `desktop-shell.md` §7 always said opens the overview;
    //     M8 Part D made it advance to the next desktop only because there was no overview yet.
    //
    //     Bring the terminal back to this desktop first: `work` is empty after 6c, and an
    //     overview of nothing has no thumbnail to drag.
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    // **The compositor's own line first, and it comes first in the guest too**: the shell
    // captures every visible window *before* it creates the overview to show them in. Asserted
    // rather than inferred, because an overview that opened with no thumbnails would satisfy
    // every assertion below about windows moving — the drag is by coordinate.
    session.expect("compositor: captured window ")?;
    // **And it wrote pixels.** The compositor logs a successful capture whether or not the
    // scale put anything in the buffer, and a black thumbnail is indistinguishable from a dark
    // window on a serial console — so the shell checks the buffer it owns and says what it
    // found. Without this the gate passed against a compositor that answered `Ok` and wrote
    // nothing at all.
    session.expect("desktop-shell: thumbnail of window ")?;
    session.expect("desktop-shell: overview open, window ")?;
    // **How many cards there are, read rather than assumed** (desktop refresh, Part E). The
    // block of cards is centred, so every card's position depends on the count — and the count
    // is a fact about this desktop's *state*, which the gate cannot compute. The line reads
    // `<id> showing <k> of <n> desktops`. Where each card then *sits* is the gate's own
    // arithmetic, kept here on purpose (M11 decision 2).
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    // Press inside the terminal's box in the **current** desktop's card, which is the first —
    // 6d switched to `work` with the chord above. Release over the second card, which is the
    // desktop the window is being moved to.
    //
    // **Aimed from the window's real geometry**, which the shell reported when it placed it:
    // a card draws every window where it actually is, so unlike the old fixed grid there is no
    // cell to aim at, and a hard-coded point would be a point that moved with the cascade.
    let thumb = size.window_in_card(0, cards, (term_x, term_y, term_w, term_h));
    // **The drag starts from a position already verified — by the click that opened this.** A
    // drag cannot check its own start: there is no press receipt until the button goes down, and
    // by then it has begun. The name's `click_at` above asserted where it landed and left the
    // pointer there, and opening the overview does not move it, so the walk to the thumbnail is
    // the same arithmetic every other step here does.
    //
    // **This used to be a second `click_at`, and there is nowhere left to aim one.** It was on
    // the thumbnail while a pick-up-and-abandon changed nothing, then on empty background while
    // that did nothing either. As of 2026-08-26 a click on a thumbnail raises its window and a
    // click on the background dismisses — which is the point of those changes, and leaves a
    // verifying click with no inert place to land.
    move_pointer_to(qmp, thumb.0, thumb.1)?;
    qmp.pointer = Some(thumb);
    qmp.send_button("left", true)?;
    // The press is the last event injected, so QEMU may hold it — see `expect_after_pointer`.
    expect_after_pointer(qmp, session, "desktop-shell: dragging window ")?;
    let (dx, dy) = size.card_click(1, cards);
    move_pointer_to(qmp, dx, dy)?;
    qmp.pointer = Some((dx, dy));
    qmp.send_button("left", false)?;
    expect_after_pointer(qmp, session, "desktop-shell: dropped window ")?;
    session.expect("desktop-shell: overview closed")?;

    // And it really moved: `work` is empty again, and the window is on the desktop it was
    // dropped on. **`(empty)` rather than the bare prefix**, which matched a list still holding
    // the window just as happily as one that had lost it (PR #244 review, optional 6).
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // **A window clicked in a card that is not the current desktop's** (PR #323 review,
    // blocking 1). The state here is exactly the one that matters: `work` is current and empty,
    // and the terminal is on `cli` — so the terminal's box is in a card the user is not on.
    //
    // Before the fix this raised a window on a desktop nobody was looking at and closed the
    // overview, leaving the screen unchanged: `WindowStack::raise` reorders a stack, and
    // nothing had changed which desktop was being composited. **No step reached it**, because
    // the card click above aims at `card_click` — "where the placement cascade never reaches" —
    // and the raise click below only ever ran once its desktop was already current.
    //
    // The switch is asserted *before* the raise because that is the order the shell does it in:
    // a raise on a desktop that is not being composited is the bug, not the fix.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let away = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, away.0, away.1)?;
    // **`Desktop 2`, not `cli`**: the second desktop is not named until 6f, so at this point its
    // label is the positional one `desktop_label` gives an unnamed desktop.
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;
    println!("  ok: a window in another desktop's card went to that desktop and raised it");

    // Back to `work`, so the rest of this step runs from where it used to.
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // 6e. **`/dev/desktop` and its first consumer** (M8 Part F).
    //
    //     the `desktop-endpoint` deferral refused to bind an endpoint nothing resolved, because this
    //     milestone had three times shipped a capability that was specified, tested in isolation
    //     and unreachable on the path a caller uses. So the binding and something that reaches
    //     it land together — and *this* is the assertion that says so: a `/bin` command, run by
    //     the shell that `nxterm` spawned, resolving a path bound into a namespace
    //     `desktop-shell` constructed.
    //
    //     Typed at the terminal, which is the only way a program in this session is started.
    //     Its stdout goes into the terminal's grid, which renders under `test-harness` only —
    //     so the command also says what it did on the debug console, which is where every gate
    //     reads a release image.
    //
    //     The terminal has the keyboard: it was raised and focused above, and the overview is
    //     closed.
    // **Click inside the terminal first, which is the two-focus rule.** Raising it from the bar
    // gives the *window* the keyboard; the grid *widget* inside it is focused by `libui`'s
    // router, and that happens on a press. Without this the keys arrive at `nxterm` — verified,
    // 28 of them — and its router has nowhere to send them, so nothing reaches the shell and
    // the command simply never runs. `check-terminal` has always clicked into the terminal
    // before typing; this gate had not needed to until now.
    click_at(qmp, session, 200, 200)?;
    type_at_terminal(qmp, "desktop")?;
    // **The shell answering, and the command reporting** — both halves, because either alone
    // is satisfiable without the other. A shell that served a list nobody received would print
    // the first; a command that invented an answer would print the second.
    // **Only the shell's own lines are ordered here.** The command runs in its own process, so
    // whether its output lands before or after the shell's next line is a race between two
    // processes — one run put `desktop: named` before the bar's redraw and the next put it
    // after. What the command said is checked against the whole transcript below, where order
    // does not matter; this is the same rule PR #227's review established for `nxterm`.
    session.expect("desktop-shell: served List of ")?;

    // And a mutation, which is the half a read-only op cannot prove: the command changes the
    // shell's model, and the shell's own bar says so.
    // **The desktop showing, which is the second one** — the drop moved the terminal there and
    // `Super+2` followed it. Naming the *first* would change a desktop the bar is not showing,
    // and the assertion below reads the bar.
    type_at_terminal(qmp, "desktop name 2 cli")?;
    session.expect("desktop-shell: served Name cli")?;
    // The bar is the shell's own readout, so this is the model changing rather than a reply.
    session.expect("desktop-shell: window list on cli of ")?;

    // **And the chrome is still there.** Both bars are `panel`s created at startup, so the
    // compositor stamped them with the desktop that was current then — and `visible_on` is the
    // single predicate behind compositing, focus *and* hit-testing, so from the first switch
    // they were neither drawn nor clickable. The only way back to the applications button was a
    // chord (PR #243 review, blocking 1). The shell marks them sticky; this is what says so.
    //
    // Asserted by the compositor naming the window a press landed on: on another desktop a
    // non-sticky bar gives `win=none`, and no shell line follows because nothing was reached.
    //
    // **The desktop's name opens the overview**, as the indicator in its place did since M8
    // Part E — what `desktop-shell.md` §7 always specified. Escape closes it again so the serial login below is not typing at a
    // popup that holds the keyboard.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    // **Before the "overview open" line, because that is where it is** — `open_overview` reports
    // its ground and the caller announces the window afterwards. An expectation placed beside
    // its *topic* rather than beside its position in the stream scans past output that was
    // there, which is the rule this file states about the theme and re-learns here.
    //
    // **The overview keeps the desktop's picture** (reported from a real session, 2026-09-02).
    // It is a full-screen *opaque* window — a translucent one has been possible since M13 Part
    // B, but the overview is not one yet (Part C) — so it
    // does not sit over the desktop, it replaces it, and painting a flat colour made the
    // wallpaper disappear whenever you looked at the desktops. It is the wallpaper dimmed now,
    // and each sidebar miniature is the wallpaper scaled, because a preview showing flat blue
    // while the desktop behind it shows a photograph is a preview of something that does not
    // exist.
    //
    // **The line, not the pixels.** A release-image boot has no reference render to compare a
    // photograph against; what this pins is that the picture was kept, passed down and used.
    // `cargo xtask shot` is what shows the result, which is what that tool is for.
    session.expect("desktop-shell: overview ground is the wallpaper")?;
    println!("  ok: the overview kept the desktop's picture rather than covering it");
    session.expect("desktop-shell: overview open, window ")?;
    press(qmp, "esc")?;
    session.expect("desktop-shell: overview closed")?;

    // **And so is the wallpaper, which it was not** (M12 Part F; PR #272 review, blocking 1).
    // It is created at startup like the bars, so the compositor stamped it with desktop 1 and
    // every other desktop showed the bare ground colour — visible as flicker rather than as a
    // missing feature, because switching back restored it.
    //
    // **Asserted on an empty desktop**, which is what makes the press unambiguous: `Super+3`
    // has nothing on it, so every point that is not one of the two bars is the wallpaper or is
    // nothing at all. A press in the middle names the wallpaper's window when it is sticky and
    // gives `win=none` when it is not — the same discriminator the bars use above, and the same
    // reason: the shell's own "is sticky" line says it *asked*, and the compositor's says it
    // took.
    chord(qmp, false, "3")?;
    session.expect("desktop-shell: switched to ")?;
    // **Empty is asserted, not assumed** — the discriminator only works where nothing else is
    // under the cursor, and which desktop holds what has moved several times by this point.
    session.expect("(empty)")?;
    // **`click_at` hands back the press line's tail**, which is the `win=` it ends with; an
    // `expect` for the whole line would scan past its own evidence. (It did, on this step's first
    // run.) Returned rather than read here, because `click_at` now also waits for the *release*
    // and that wait moves the cursor past the press line — a `rest_of_line` here would read the
    // release's `win=`, which is `none` by construction.
    let hit = click_at(qmp, session, size.centre().0, size.centre().1)?;
    if !hit.contains(&format!("win={wallpaper_id}")) {
        return Err(format!(
            "a press on an empty desktop reported '{}' — the wallpaper (window {wallpaper_id}) \
             is not sticky, so every desktop but the first shows the bare ground colour",
            hit.trim()
        )
        .into());
    }
    println!("  ok: the wallpaper is sticky — still there on an empty desktop");
    // Back to the one the rest of this gate is written against.
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;

    // 6f. **The overview's clicks, which were dead until 2026-08-26.** `desktop-shell.md` §6
    //     says "you can switch desktops from inside it"; a press on a sidebar row set no drag
    //     and its release matched no arm, and a press-and-release on a thumbnail was discarded
    //     as an abandoned drag. So the two obvious gestures — go to that desktop, go to that
    //     window — did nothing, and only the *drag* the gate above exercises was ever wired up.
    //     Reported from a real session, which is the part worth keeping: the drag was gated and
    //     the click was not, and a gate that drives only the gesture it was written for cannot
    //     tell the difference between "unimplemented" and "untested".
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    // The chord path first: an overview left showing the desktop you just switched away from is
    // showing a card whose windows are no longer on it. It follows instead of closing,
    // which is what §6 means by "it fetches a different set of images".
    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    session.expect("desktop-shell: overview now showing 0 on work")?;

    // Then a click on a card, with no drag in flight. Card 1 is the second desktop — `cli`,
    // which is where the terminal is — so the refresh must find it again.
    //
    // **Aimed at the foot of the card**, clear of its border and of any window drawn in it:
    // this click means "go to that desktop", and one that landed on a window would mean "raise
    // that window" instead — which is the distinction the cards made possible and the reason
    // this point is not simply the card's middle.
    let (sx, sy) = size.card_click(1, cards);
    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: switched to cli")?;
    session.expect("desktop-shell: overview now showing 1 on cli")?;

    // **Clicking the row you are already on dismisses**, which is the way out of an overview on
    // a desktop with no windows — where clicking a window, the other way out, does not exist.
    // Reported as being stuck there with Escape the only escape, and Escape is not discoverable.
    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: overview closed")?;

    // **And a click on its background dismisses**, the way clicking outside a menu does. That
    // also makes the desktop's name a toggle: the overview covers the bar, so a second click
    // where the name is lands on background.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    click_at(qmp, session, 600, 700)?;
    session.expect("desktop-shell: overview closed")?;

    // And a click on a window in a card activates it, which is the third way out and the one
    // that takes you somewhere. `raise_window` is the same call the window list's entries make.
    //
    // **The terminal is on `cli` now**, which the clicks above switched to, so its box is in
    // whichever card that is — found by the same arithmetic as the drag, against the geometry
    // the shell reported when it placed the window.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let on_term = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, on_term.0, on_term.1)?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;

    Ok(())
}

/// `check-login`'s steps 6f to 6k and the second session: the overview's clicks, dragging,
/// resizing and snapping windows.
pub(crate) fn login_overview_drags(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let term_id = st.term_id;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let work = st.work;
    // 6g. **A window is dragged by its own title bar** (M9 Part A). Client-side decorations mean
    //     the bar is pixels `nxterm` committed; what crosses the wire is one `StartMove`, and the
    //     compositor — which is holding the grab the press opened — moves the window from there.
    //
    //     **The motion is injected before the request can arrive — probabilistically, and worth
    //     saying so.** The two motions are *sent* before the wait below, but nothing proves the
    //     compositor processed them before the client's request: they travel QMP → PS/2 →
    //     `input-server` while the request travels compositor → client → compositor, and the
    //     ordering is a race this gate wins by a wide margin rather than by construction. If
    //     that ever stops being true the step still passes for the correct implementation and
    //     stops distinguishing the late-measurement one — so the host test
    //     `an_interactive_move_offsets_by_where_the_press_landed…` is the guard that cannot
    //     drift, and this is the one that proves the whole path (PR #248 review, finding 9).
    //
    //     **The motion is injected before the request can arrive, and that is the assertion.**
    //     `StartMove` is a full round trip after the press: the compositor delivers it, `libui`
    //     routes it, `nxterm` decides it landed on the bar, and only then does the request go
    //     out. A compositor that took its drag offset from the pointer *at the request* would
    //     lose whatever the pointer did in between — the window jumps by that much and then
    //     tracks correctly, which is exactly the defect the `scroll-grab` deferral described. Every
    //     other step here waits for the guest between injections; this one deliberately does not,
    //     because a stationary pointer across that round trip measures zero drift where a person
    //     sees forty pixels (PR #247 review, finding 4).
    const DRAG_STEPS: i32 = 4;
    const DRAG_DX: i32 = 10;
    const DRAG_DY: i32 = 5;
    // The title bar is the top of the window; x=100 is clear of the buttons at its right.
    let press_at = (100, term_y + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    // **Half the motion before the request can possibly arrive**, which is the half a
    // compositor reading the pointer at `StartMove` would lose.
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    // **And the button stays down until the drag is accepted.** A person holds it for a
    // fraction of a second; this harness can press and release faster than the round trip to
    // the client and back, and a release that lands first takes the grab away — the compositor
    // then refuses a move for a window nobody is holding, which is correct and is not what this
    // step is testing (found the expensive way: the first version released immediately).
    session.expect("compositor: interactive move of window ")?;
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + DRAG_STEPS * DRAG_DX, press_at.1 + DRAG_STEPS * DRAG_DY));
    // The window ended up offset by exactly what was injected. One line for the gesture:
    // the compositor reports no geometry change per motion, deliberately.
    // **From where it started, not from zero.** A drag moves a window by what was injected, and
    // the destination is its origin plus that — which was the same number only while the cascade
    // placed every window at x=0 (M11 Part E batch 4).
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} ",
            term_x + DRAG_STEPS * DRAG_DX,
            term_y + DRAG_STEPS * DRAG_DY
        ),
    )?;
    println!("  ok: the terminal moved with its own title bar, offset by where it was grabbed");

    // **And maximising it now *moves* it, which is the half a `Configure` was not applying.**
    // The window is at (40, …) after the drag, so a maximise that only carried a size would
    // resize it in place and leave it hanging off the screen. The compositor applies the origin
    // as `Place` would (PR #249 review, blocking 2).
    //
    // **The size is asserted here too, as of M9 Part D** — this is where Part B's box was left
    // open, deliberately, because the size stays the client's to decline and `nxterm` declined
    // every `Configure` for three milestones. Now it accepts, and the assertion is the whole
    // round trip: the button asks, the shell decides, the compositor forwards, the client
    // reallocates and *commits* at the new size, and the geometry the shell reads back is the
    // committed one. A client that still declined would produce every line but the last.
    //
    // The button is at the window's top-right, and the window has moved: its origin is where it
    // was placed plus what the drag injected.
    let moved_x = term_x + DRAG_STEPS * DRAG_DX;
    let moved_y = term_y + DRAG_STEPS * DRAG_DY;
    click_at(qmp, session, moved_x + term_w as i32 - chrome::MAXIMISE_X, moved_y + chrome::TITLE_Y)?;
    // **Two producers, one cause, so no order between them.** The shell logs the window's new
    // origin when the compositor's geometry event reaches it; the client logs the size it took
    // when the `Configure` reaches it. Both are downstream of the same apply and neither is
    // downstream of the other, so on four vCPUs either can reach the console first. Asserted as
    // a sequence this passed for two milestones and then failed in CI with both lines present
    // and the *first* expect having scanned past the second (2026-08-31).
    //
    // **And the request and the shell's answer to it in the same wait** (PR #353's CI): `nxterm`
    // logs the request after the compositor answers it, which is after the shell has it, so the
    // shell's maximise *and its move* can both come before the client's line. Waited for apart,
    // the first wait's cursor jumps past the move to the request, and the second takes the final
    // geometry for the move.
    //
    // The client's line says what it did with the size *and* what that came to in cells, which
    // is the difference between a window that grew and a terminal that can use the room: a grid
    // still 80x24 in a 1280x752 window would satisfy every other line here.
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    let moved = format!("desktop-shell: window {term_id} geometry {},{} ", work.0, work.1);
    let took = format!("nxterm: resized to {}x{}, grid ", work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised, &moved, &took])?;
    // And the committed geometry — the one the compositor reports and `/dev/draw/<id>/info`
    // answers with — is the work area exactly, not the work area rounded down to whole cells.
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    println!("  ok: maximise moved the window and the client committed the work area exactly");

    // 6j. **The window is dragged smaller by its own corner** (M9 Part E). The grip is pixels
    //     `nxterm` committed, like the title bar; what crosses the wire is one `StartResize`,
    //     and then nothing at all until the button comes up. The compositor moves an outline —
    //     its own drawing, over the composed stack, reaching no client — and hands the shell one
    //     rectangle at the release.
    //
    //     **The window is maximised here, so its corner is the work area's**, and dragging
    //     inward is what keeps every injected motion inside the screen: a drag that ran into the
    //     clamp would assert against a rectangle the pointer never reached.
    //
    //     **Four assertions, in the order the mechanism goes**: the compositor took the gesture
    //     and says which edges; it reports one rectangle when the button comes up; the *shell*
    //     turns that into a `Configure`, which is the half that proves the compositor did not
    //     resize anything itself; and the client commits it. The last is the one the plan calls
    //     for — the control is an outline that tracks and a release that commits nothing, and
    //     every line before the last holds for that implementation too.
    const RESIZE_STEPS: i32 = 4;
    const RESIZE_DX: i32 = -50;
    const RESIZE_DY: i32 = -25;
    let resized_w = (work.2 as i32 + RESIZE_STEPS * RESIZE_DX) as u32;
    let resized_h = (work.3 as i32 + RESIZE_STEPS * RESIZE_DY) as u32;
    // The grip is a 16-pixel square over the window's bottom-right corner; its middle is eight
    // pixels in from each edge.
    let grip_at = (work.0 + work.2 as i32 - 8, work.1 + work.3 as i32 - 8);
    move_pointer_to(qmp, grip_at.0, grip_at.1)?;
    qmp.pointer = Some(grip_at);
    qmp.send_button("left", true)?;
    // `RESIZE_RIGHT | RESIZE_BOTTOM` is `2 | 8`.
    expect_after_pointer(qmp, session, &format!("compositor: interactive resize of window {term_id} edges 10"))?;
    for _ in 0..RESIZE_STEPS {
        qmp.send_motion(RESIZE_DX, RESIZE_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((grip_at.0 + RESIZE_STEPS * RESIZE_DX, grip_at.1 + RESIZE_STEPS * RESIZE_DY));
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "compositor: interactive resize of window {term_id} ended at {},{} {resized_w}x{resized_h}",
            work.0, work.1
        ),
    )?;
    // **The shell is what resizes the client**, which is the whole reason the gesture ends in an
    // event rather than in a `Configure` from the compositor: one path to a window's geometry
    // rather than two that can disagree. It says `drop` rather than `resize` because since
    // Part F one event ends both gestures, so the shell has one word for what it is answering —
    // the user let go, and this is the rectangle.
    session
        .expect(&format!("desktop-shell: drop window {term_id} to {},{} {resized_w}x{resized_h}", work.0, work.1))?;
    session.expect(&format!("nxterm: resized to {resized_w}x{resized_h}, grid "))?;
    // And the committed geometry, which is the assertion a release that commits nothing fails.
    session
        .expect(&format!("desktop-shell: window {term_id} geometry {},{} {resized_w}x{resized_h}", work.0, work.1))?;
    println!("  ok: the terminal was dragged smaller by its corner, and committed it");

    // 6k. **A window thrown at the left edge snaps to half the work area** (M9 Part F). The
    //     shell registered eight zones — four edges, four corners — computed from the work area,
    //     and the compositor matches the pointer against that table during a move. It knows
    //     nothing about halves: what it shows and what it asks for is the *target* rectangle the
    //     table gave it, which is why the policy can be wrong only in the shell.
    //
    //     **The assertion is against the work area, not the screen.** A zone table computed from
    //     `screen_h` would put the window under the bars, and the committed geometry is the only
    //     line that says which of the two the shell used.
    //
    //     The window is `resized_w x resized_h` at the work area's origin after 6j, so its title
    //     bar is at `work.1 + chrome::TITLE_Y` and clear of the buttons at `x = 100`.
    // **First the control, as a step of its own with a positive assertion**: a drag that passes
    // *through* a zone and is released outside it must snap nothing — and what it must do
    // instead is the ordinary move, so the assertion is the geometry that move produces. A
    // compositor that snapped on entry rather than on release would report the zone's target
    // here, and this line is what catches it. Asserting the absence of a snap would not: the
    // step after this one produces exactly that line, and an `expect` scans forward.
    let press_at = (100, work.1 + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(qmp, session, "compositor: interactive move of window ")?;
    // **The expectation is summed from what is injected**, not written out beside it: the first
    // version divided each leg into three and wrote the leg's total, which is two pixels away
    // from three times a truncated third.
    let mut walked = (0, 0);
    for (dx, dy) in [(-30, 60), (-30, 60), (-30, 60), (66, 0), (66, 0), (66, 0)] {
        qmp.send_motion(dx, dy)?;
        walked = (walked.0 + dx, walked.1 + dy);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    let passed_to = (press_at.0 + walked.0, press_at.1 + walked.1);
    qmp.pointer = Some(passed_to);
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} {}x{}",
            work.0 + walked.0,
            work.1 + walked.1,
            resized_w,
            resized_h
        ),
    )?;
    println!("  ok: a drag through the zone and out again moved the window and snapped nothing");

    // Then the drop itself, from the title bar where the window now is.
    //
    // **The press is asserted to land on *that* window**, which is what makes the step above a
    // control rather than a hope: a compositor that snapped on entry has moved the terminal to
    // the left half, so this press lands on nothing — and the failure says so here, one line
    // after the assertion it belongs to, instead of surfacing as a missing drag further down.
    let press_at = (passed_to.0 + 90, passed_to.1);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(
        qmp,
        session,
        &format!("compositor: press at x={} y={} win={term_id}", press_at.0, press_at.1),
    )?;
    // **On its title bar specifically**, which is the sharpest statement of where the window is:
    // a terminal that had snapped on the pass-through above sits at the work area's origin, so
    // this press lands on its *grid* — inside the window, and no drag at all. Without this line
    // the control's failure surfaces two steps later as a drag that never began.
    // In either order: a client's request and the compositor's receipt of it, as the file drag's
    // two lines are (Phase 6 Part B).
    session.expect_all(&["nxterm: dragging its own title bar", "compositor: interactive move of window "])?;
    // **Paced, and the reason is worth keeping.** The first version injected thirty-six motions
    // as fast as QMP would take them; the consumer ring overran, `input-server` announced the
    // gap, and `libinput` turned it into a `Logical::Dropped` — which ends a gesture *without*
    // asking for anything, exactly as Part E's review required. So the gate lost its drag to
    // the machinery working correctly. A person dragging a window produces nothing like this
    // rate; a harness does, and it has to slow down rather than be accommodated.
    // **Down as well as left**, because the *pointer* picks the zone and the title bar sits
    // inside the top-left **corner**'s band — the window is at the work area's origin, so a
    // straight drag left snaps a quarter, correctly, and would say nothing about edges. The
    // corners are registered first precisely so the more specific zone wins, which is the
    // manager's ordering to get right; this step is about the edge, so it aims at one.
    let step = -(press_at.0 - 10) / 6;
    for _ in 0..6 {
        qmp.send_motion(step, 30)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + step * 6, press_at.1 + 180));
    // The compositor asks for the zone's target; the shell answers with the `Configure`; the
    // client commits it. Half the **work area**, at its origin.
    expect_after_pointer(
        qmp,
        session,
        &format!("desktop-shell: drop window {term_id} to {},{} {}x{}", work.0, work.1, work.2 / 2, work.3),
    )?;
    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2 / 2, work.3))?;
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0,
        work.1,
        work.2 / 2,
        work.3
    ))?;
    println!("  ok: dropped at the left edge, the terminal took half the work area");

    // 6h. **Movement is not lost while the compositor is busy** — the one property the whole
    //     input path exists to keep, and the one nothing here checked.
    //
    //     A relative pointer's deltas *are* its position: a batch the input server cannot
    //     deliver is movement no consumer can ever re-derive, unlike a key or a button, whose
    //     state `SYN_DROPPED` asks the consumer to resynchronise. Dropping one therefore leaves
    //     the guest's cursor permanently offset from the host's pointer — which is what a
    //     person sees as "I cannot reach the left edge any more", because the host pointer
    //     leaves the window before the guest's cursor arrives (2026-08-26).
    //
    //     Injected **against a desktop switch**, which is the one repaint that legitimately
    //     costs the whole screen — ~100 ms under TCG, during which the compositor reads no input
    //     and the consumer's ring fills.
    //
    //     **Under TCG only, and that is why this is not the guard.** The overrun here depends on
    //     out-injecting a repaint, and under KVM the repaint is fast enough that 120 QMP-paced
    //     motions never fill the ring — so this step passes whether or not motion is deferred,
    //     which is what it did when it *was* the guard (PR #246 review, blocking 1). CI runs
    //     `--kvm` everywhere. The property is gated in `check-input`, where the stall is a
    //     consumer that stops reading and therefore means the same thing at any speed; what
    //     this step still buys is the whole path — a real manager, a real desktop switch, a
    //     real cursor — on the accelerator a person actually runs locally.
    burst_holds_its_position(qmp, session, &chord)?;

    // 6i. **The close button is the client's own, and sends nothing** (M9 Part C). Everything
    //      else on this bar asks the shell; this one exits. The window **is** the work area
    //      after 6g — origin *and* size, since M9 Part D — so its top-right corner is the work
    //      area's, and that is what the button is measured from.
    //
    //      **After 6h, not before it**, which costs a desktop switch back: 6h's stall is a
    //      whole-screen recompose, and a recompose with no window left in it is a weaker one
    //      than the step's own rationale describes. Closing the terminal first made the local
    //      diagnostic quieter for nothing (PR #251 review).
    //
    //      **Asserted by what is absent as well as what is present.** `nxterm: closing` says it
    //      went; the transcript check below says no `RequestClose` was sent for *this* window,
    //      which is what makes it the button rather than the taskbar. The two terminals have
    //      different ids precisely so that check can name one of them.
    //
    //      **Measured from the snapped width**: 6j dragged the window smaller by its corner and
    //      6k then dropped it at the left edge, so its right edge is the work area's midpoint.
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    click_at(qmp, session, work.0 + (work.2 / 2) as i32 - chrome::CLOSE_X, work.1 + chrome::TITLE_Y)?;
    session.expect("nxterm: closing")?;
    session.expect("desktop-shell: window list on ")?;
    println!("  ok: the close button closed the terminal with no request to the shell");

    // 4. **Two independent sessions**, which is Part D's fourth box and the one most easily
    //    asserted rather than tested. The graphical session is running *now* — its leader
    //    blocks, so it does not end on its own — and the serial column's prompt has been live
    //    on this same boot the whole time. Logging in there proves the two supervisors do not
    //    contend: neither arbitrates, there is no registry, and serial stays the recovery
    //    path by construction rather than by care.
    //
    //    The same user, twice, with two namespaces. That cost is named and accepted in
    //    `graphical-session.md` §6.2 — it matches Linux, where `getty` and `gdm` do not
    //    coordinate either.
    session.send(DEMO_USER)?;
    session.expect("password:")?;
    session.send(DEMO_PASSWORD)?;
    session.expect("/home>")?;

    Ok(())
}
