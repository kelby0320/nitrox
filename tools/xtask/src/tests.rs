//! xtask's unit tests.

use super::*;

/// **The throughput stick's tree**: its three parts at the sizes the plan names, 384 MiB in all,
/// and every name once — which is what lets a copy be checked by its name wherever it was put.
#[test]
fn the_throughput_tree_is_the_plans_and_its_names_are_unique() {
    let files = super::throughput_files();
    let in_dir = |d: &str| files.iter().filter(|f| f.0 == d || f.0.starts_with(&format!("{d}/"))).count();
    assert_eq!((in_dir("big"), in_dir("small"), in_dir("tree")), (32, 4096, 1024));
    assert_eq!(files.iter().map(|f| f.2).sum::<u64>(), 384 << 20);
    let names: std::collections::HashSet<&String> = files.iter().map(|f| &f.1).collect();
    assert_eq!(names.len(), files.len(), "a name twice could not be checked by its name");
    let short = |n: &str| {
        n.split_once('.').is_some_and(|(b, e)| b.len() <= 8 && e.len() == 3)
            && n.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'.')
    };
    assert!(files.iter().all(|f| short(&f.1)), "every name a FAT short name, as mcopy needs");
}

/// A pattern is its name's alone, the same each time, and its length to the byte.
#[test]
fn a_throughput_pattern_is_its_names_and_its_lengths() {
    let a = super::throughput_pattern("S0001.BIN", 16 * 1024);
    assert_eq!(a, super::throughput_pattern("S0001.BIN", 16 * 1024));
    assert_ne!(a, super::throughput_pattern("S0002.BIN", 16 * 1024));
    assert_eq!(super::throughput_pattern("x", 13).len(), 13);
    assert_eq!(&super::throughput_pattern("x", 13)[..], &super::throughput_pattern("x", 64)[..13]);
}

/// **The manager lists at least the first round's devices** (PR #356 review): the lines the
/// three runs of fgb68 and before gave pass, and the reviewer's control — a manager that
/// dropped every USB record, which `boot-probe` passed — fails, as does one short by one.
#[test]
fn device_mgr_must_list_the_first_rounds_usb_devices() {
    let boot_with = |listed: usize, since: usize, handed: usize| {
        format!(
            "usb: port 9: keyboard at /dev/input/raw/2\nusb: port 10: mouse at /dev/input/raw/3\n\
                 usb: first round: 5 device(s) in 287 ms\ninit: spawned init (pid 1)\n\
                 input-server: {handed} device(s) from the device manager\nboot-probe: devices: \
                 block held by the storage service, all.tsm has 20 rows, {listed} of them usb-<id>, and \
                 {since} USB records since ok\n"
        )
    };
    let boot = |listed: usize, since: usize| boot_with(listed, since, 4);
    for (listed, since) in [(5, 2), (6, 1), (7, 0)] {
        assert!(check_usb_listed(boot(listed, since).as_bytes()).is_ok(), "{listed} listed");
    }
    assert!(check_usb_listed(boot(0, 7).as_bytes()).is_err(), "none listed: the reviewer's control");
    assert!(check_usb_listed(boot(4, 3).as_bytes()).is_err(), "one short");
    let no_round = boot(5, 2).replace("usb: first round: 5 device(s)", "usb: no round");
    assert!(check_usb_listed(no_round.as_bytes()).is_err(), "no first round to hold it to");
    // **`input-server` handed the first round's keyboard and mouse** beside the i8042's two
    // (Part B.2): five when a hot-plugged keyboard was bound before the manager's read, and
    // three is one short.
    assert!(check_usb_listed(boot_with(5, 2, 5).as_bytes()).is_ok(), "a hot-plugged keyboard too");
    assert!(check_usb_listed(boot_with(5, 2, 3).as_bytes()).is_err(), "one input device short");
}

#[test]
fn a_settled_receipt_is_the_last_whole_one() {
    // The approach crosses `Clear` and stops on `Copy`: the answer is where it stopped.
    let text = "nxterm: menu hover 106\nheartbeat\nnxterm: menu hover 100\n";
    assert_eq!(last_receipt(text, "nxterm: menu hover "), (Some("100".to_string()), text.len()));
    // A line still being written is not read, and not consumed.
    let partial = "nxterm: menu hover 100\nnxterm: menu hover 10";
    assert_eq!(
        last_receipt(partial, "nxterm: menu hover "),
        (Some("100".to_string()), "nxterm: menu hover 100\n".len())
    );
    // Nothing complete, nothing read.
    assert_eq!(last_receipt("nxterm: menu hov", "nxterm: menu hover "), (None, 0));
    assert_eq!(last_receipt("heartbeat\n", "nxterm: menu hover "), (None, 10));
}

/// **The lights after the last reset, read from a real trace** (Phase 6 Part B.5): the firmware's
/// reset and its lights, then the kernel's reset, its zero and the compositor's Num Lock. The
/// first version split at the word `ledstate`, which the event's name holds too, and read
/// nothing from every line — the gate's first run said `[]` with this trace on disk.
#[test]
fn the_lights_are_read_after_the_last_reset() {
    let trace = "ps2_reset_keyboard 0x5fe490c46268\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 0\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 4\n\
                     ps2_reset_keyboard 0x5fe490c46268\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 0\n\
                     ps2_set_ledstate 0x5fe490c46268 ledstate 2\n";
    assert_eq!(super::ps2_lights_in(trace), [0, 2]);
    assert_eq!(super::ps2_lights_in(""), [0u8; 0], "no trace, no lights");
}

#[test]
fn the_staged_theme_reads_back_as_itself_and_only_its_overrides_are_live() {
    let (shipped, text) = super::staged_theme().unwrap();
    let (read, issues) = libdraw::theme::Theme::from_config(&text);
    assert!(issues.is_empty(), "{issues:?}\n{text}");
    assert_eq!(read, shipped);
    // **Live lines are exactly the deliberate ones**: the scheme, and the three fields a gate
    // reads back. Every colour is shown but commented, which is what lets the scheme line do
    // anything at all.
    let live: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split(" =").next())
        .collect();
    assert_eq!(live, ["scheme", "font_px", "wallpaper", "wallpaper_mode"], "{text}");
    assert!(text.contains("\n# accent = \"#2C7F92\"\n"), "the colours are shown:\n{text}");
    // And changing that one line changes the palette, keeping the overrides.
    let dark = text.replace("scheme = \"light\"", "scheme = \"dark\"");
    let (read, issues) = libdraw::theme::Theme::from_config(&dark);
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(read.background, libdraw::theme::Theme::dark().background);
    assert_eq!((read.font_px, read.wallpaper), (shipped.font_px, shipped.wallpaper));
}

/// Every metric `chrome` copies from `libui::widget` still equals the toolkit's.
///
/// **The generalisation of the greeter's pair** (PR #318 review, finding 1, applied to the
/// rest): `chrome` exists because a gate that read the toolkit to know where to click could
/// agree with a toolkit that had stopped drawing where it says (M11 decision 2) — and a
/// deliberate copy still needs something that fails when it goes stale. `libui`'s own tests
/// pin the other side, pressing these literals on trees that are actually built.
///
/// **Imported rather than parsed.** This read the constants out of `widget.rs` with a little
/// parser and a guard against the pattern going stale; `xtask` links `libui` already — it
/// renders the display gate's reference frames with it — so the comparison is just a
/// comparison (PR #319 review, optional 8). Nothing about M11 decision 2 is weakened: a
/// *gate* still aims from `chrome`, and only this test sees both sides.
///
/// **Only the direct copies.** `TITLE_Y`, `CLOSE_X`, `MAXIMISE_X`, `MINIMISE_X` and
/// `CONTENT_X` are sums over the toolkit's constants rather than any one of them, and
/// `the_title_buttons_land_where_the_gates_aim` is what pins those.
#[test]
fn the_gates_chrome_table_is_the_toolkits() {
    use libui::widget as w;
    for (what, gate, toolkit) in [
        ("TITLE_BAR_H", chrome::TITLE_BAR_H, w::TITLE_BAR_H as i32),
        ("TAB_STRIP_H", chrome::TAB_STRIP_H, w::TAB_STRIP_H as i32),
        ("TAB_H", chrome::TAB_H, w::TAB_H as i32),
        ("TAB_TOP", chrome::TAB_TOP, w::TAB_TOP as i32),
        ("TAB_W", chrome::TAB_W, w::TAB_W as i32),
        ("TAB_SIDE", chrome::TAB_SIDE, w::TAB_SIDE as i32),
        ("TAB_PITCH", chrome::TAB_PITCH, w::TAB_PITCH as i32),
        ("DIALOG_W", chrome::DIALOG_W, w::DIALOG_W as i32),
        ("DIALOG_H", chrome::DIALOG_H, w::DIALOG_H as i32),
        ("DIALOG_LEFT_CX", chrome::DIALOG_LEFT_CX, w::DIALOG_LEFT_CX),
        ("DIALOG_RIGHT_CX", chrome::DIALOG_RIGHT_CX, w::DIALOG_RIGHT_CX),
        ("DIALOG_BUTTON_CY", chrome::DIALOG_BUTTON_CY, w::DIALOG_BUTTON_CY),
    ] {
        assert_eq!(gate, toolkit, "`chrome::{what}` is {gate}, the toolkit's is {toolkit}");
    }
}

/// Every metric `browser` copies from `nxfiles` still equals the browser's.
///
/// **Read out of the source, because this crate cannot link that one** — `nxfiles` is a
/// bare-target program and its library half pulls `libsurface` with it, which is the same
/// reason the greeter's pair is parsed rather than imported. The toolkit's own metrics are
/// imported instead; see `the_gates_chrome_table_is_the_toolkits`.
///
/// The extraction is guarded: a constant that stops being a literal fails here rather than
/// quietly comparing nothing.
#[test]
fn the_gates_browser_table_is_the_browsers() {
    let src = repo_root().join("userspace/nxfiles/src");
    let mut text = String::new();
    for entry in fs::read_dir(&src).expect("the browser's source is readable") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("main.rs") {
            text += &fs::read_to_string(&path).unwrap();
        }
    }
    let mut found: BTreeMap<String, i32> = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        let Some((ty, val)) = tail.split_once('=') else { continue };
        if ty.trim() != "u32" {
            continue;
        }
        if let Ok(v) = val.trim().trim_end_matches(';').parse::<i32>() {
            found.insert(name.trim().to_string(), v);
        }
    }
    let get = |what: &str| {
        *found.get(what).unwrap_or_else(|| {
            panic!(
                "no `pub const {what}: u32` in {} — this checker's pattern has gone stale, \
                     which silently stops comparing anything",
                src.display()
            )
        })
    };
    for (what, gate, app) in [
        ("SIDEBAR_W", browser::SIDEBAR_W, get("SIDEBAR_W")),
        ("SIDEBAR_PAD", browser::SIDEBAR_PAD, get("SIDEBAR_PAD")),
        ("PATH_H", browser::PATH_H, get("PATH_H")),
        ("ROW_H", browser::ROW_H, get("ROW_H")),
        ("MENU_BAR_H", browser::MENU_BAR_H, get("MENU_BAR_H")),
    ] {
        assert_eq!(gate, app, "`browser::{what}` is {gate}, the browser's is {app}");
    }
    // `HEADER_H` is the toolkit's, which the browser re-exports rather than spelling.
    assert_eq!(browser::HEADER_H, libui::widget::LIST_HEADER_H as i32);
    // **The Drives heading is a row's height** (Phase 6 Part F), spelled as one there.
    assert!(
        text.contains("pub const DRIVES_HEADING_H: u32 = ROW_H;") && browser::DRIVES_HEADING_H == browser::ROW_H,
        "the browser's Drives heading is no longer a row's height, and `check-media` aims below it"
    );
}

/// **The gate's copy of the editor's chooser key is the editor's**, and the toolkit puts the
/// chooser's Up button at that key plus seven — found, in the window the toolkit sizes, in the
/// strip below the title, at the theme the image stages (Phase 6 Part F).
#[test]
fn the_gates_editor_table_is_the_editors() {
    let path = repo_root().join("userspace/nxedit/src/lib.rs");
    let text = fs::read_to_string(&path).expect("the editor's source is readable");
    let line = format!("pub const CHOOSER_KEY: u64 = {};", editor::CHOOSER_KEY);
    assert!(text.contains(&line), "no `{line}` in {}", path.display());
    let (x, y) = chooser_up_at(libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H).expect("an Up button");
    assert!(x > 0 && x < 40 && y > libui::widget::TITLE_BAR_H as i32 && y < 90, "Up is at ({x}, {y})");
}

/// The gate's copy of the greeter's window is the greeter's own.
///
/// **Two constants that must agree, kept by a test rather than by a comment** — the same
/// arrangement `abi-sync-check` uses for the kernel's constants and `libkern`'s, and for the
/// same reason: `xtask` writes chrome metrics down a second time on purpose (M11 decision 2),
/// and a deliberate copy still needs something that fails when it goes stale. Part D claimed
/// the greeter's own host test covered this; it measures the card against the *greeter's*
/// constant and never reads this one (PR #318 review, finding 1).
///
/// The extraction is guarded: finding neither constant is a failure, not a pass, because a
/// pattern that has gone stale would otherwise silently agree with everything.
#[test]
fn the_gates_greeter_size_is_the_greeters_own() {
    let path = repo_root().join("userspace/desktop-session-mgr/src/lib.rs");
    let text = fs::read_to_string(&path).expect("the greeter's source is readable");
    let mut found: Vec<(String, u32)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const GREETER_") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        let Some((ty, val)) = tail.split_once('=') else { continue };
        if ty.trim() != "u32" {
            continue;
        }
        if let Ok(v) = val.trim().trim_end_matches(';').parse::<u32>() {
            found.push((name.trim().to_string(), v));
        }
    }
    let get = |what: &str| {
        found.iter().find(|(n, _)| n == what).map(|(_, v)| *v).unwrap_or_else(|| {
            panic!(
                "no `pub const GREETER_{what}: u32` in {} — this checker's pattern has gone \
                     stale, which silently stops comparing anything",
                path.display()
            )
        })
    };
    assert_eq!((get("W"), get("H")), GREETER, "the greeter's window and this file's copy of it have drifted");
}

/// The gate's copy of the overview's card metrics is the shell's.
///
/// **The copy is deliberate** (M11 decision 2): a gate that read the shell's layout to know
/// where to aim could agree with a shell that had stopped drawing where it says. What a copy
/// still needs is this — the greeter's `340x141` was `420x200` for a milestone, and the
/// lesson Part D drew is that a copy without a comparison is a number nobody checks.
///
/// **Parsed rather than imported**, because `desktop-shell` is a bare-target crate this one
/// cannot link — the same reason `the_gates_browser_table_is_the_browsers` reads source.
#[test]
fn the_gates_card_metrics_are_the_shells() {
    let path = repo_root().join("userspace/desktop-shell/src/lib.rs");
    let text = fs::read_to_string(&path).expect("the shell's source is readable");
    let mut found: Vec<(String, i32)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        let Some((ty, val)) = tail.split_once('=') else { continue };
        if ty.trim() != "u32" {
            continue;
        }
        if let Ok(v) = val.trim().trim_end_matches(';').parse::<i32>() {
            found.push((name.trim().to_string(), v));
        }
    }
    let get = |what: &str| {
        found.iter().find(|(n, _)| n == what).map(|(_, v)| *v).unwrap_or_else(|| {
            panic!(
                "no `pub const {what}: u32` in {} — this checker's pattern has gone stale, \
                     which silently stops comparing anything",
                path.display()
            )
        })
    };
    assert_eq!(
        (
            get("CARD_W_NUM"),
            get("CARD_W_DEN"),
            get("CARD_BORDER"),
            get("CARD_GAP"),
            get("CARD_CAPTION_GAP"),
            get("CARD_CAPTION_H"),
            get("CARD_SIDE_PAD"),
            get("BAR_H"),
        ),
        (
            DisplaySize::CARD_W_NUM,
            DisplaySize::CARD_W_DEN,
            DisplaySize::CARD_BORDER,
            DisplaySize::CARD_GAP,
            DisplaySize::CARD_CAPTION_GAP,
            DisplaySize::CARD_CAPTION_H,
            DisplaySize::CARD_SIDE_PAD,
            DisplaySize::BAR_H,
        ),
        "the overview's card metrics and this file's copy of them have drifted"
    );
}

/// The shipped UI face carries the glyph a marked menu row draws.
///
/// **A missing glyph is a silent failure** — `.notdef`, drawn as a blank or a box, reported
/// nowhere — so a decorative character picked in `libui` is a claim about a font file staged
/// by this crate, and this is the only place both are in scope. The control is the second
/// assertion: a private-use codepoint no face carries, which proves the check can fail.
#[test]
fn the_menu_mark_exists_in_the_shipped_face() {
    let (ui, _) = host_faces().expect("the built-in theme's faces load on the host");
    let mark = libui::menu::MARK.chars().next().expect("MARK is not empty");
    assert!(ui.has_glyph(mark), "the UI face has no glyph for {mark:?} — a marked menu row would draw a blank");
    assert!(!ui.has_glyph('\u{E000}'), "a private-use codepoint should be absent");
}

/// Every picture the display gate demands of the guest is a preview, at the gate's size — and
/// the one preview it does not demand, `ui-dark`, is really the dark scheme.
///
/// **What this pins, and what it deliberately no longer claims to.** *Sharing* is structural
/// now: `cmd_check_display` reads its expected frames from `preview_frames`, so a preview
/// that is not the gate's picture fails against a real guest rather than passing here. This
/// test said the size assertion would catch "a different size **or theme**" and it would not —
/// a solid rectangle at 320×300 passed it (PR #261 review, finding 2).
///
/// What it does pin is worth pinning. The sizes are the gate's own constants and
/// `libterm`'s own `size()`, which is the one place those agree with the frames the gate
/// measures. And the PNG is **decoded back to the framebuffer's pixels**, which covers the
/// conversion: the toolkit reference's pitch is 1292 for a 1280-byte row and `XRGB8888` is
/// little-endian, so a direct copy is wrong twice over — a stride and a channel order.
///
/// Decoded rather than compared against what went in, because a round trip through one
/// library's encoder and back tests the encoder; what is under test here is the pixels.
#[test]
fn the_gates_pictures_are_previews_and_the_dark_one_is_dark() {
    use libdraw::framebuffer::Framebuffer;
    let faces = host_faces().expect("the vendored fonts");
    let frames = preview_frames(&faces);

    let sizes: Vec<(&str, u32, u32)> =
        frames.iter().map(|(n, f)| (*n, f.geometry().width, f.geometry().height)).collect();
    let term = libterm::render::reference::size(&faces.1);
    assert_eq!(
        sizes,
        [
            ("ui", libui::reference::WIDTH, libui::reference::HEIGHT),
            ("ui-dark", libui::reference::WIDTH, libui::reference::HEIGHT),
            ("term", term.w, term.h),
        ],
        "the previews are the gate's arrangements at the gate's sizes, and ui-dark"
    );
    // And the dark one really is dark. (60, 4) is inside the menu bar's highlighted `Edit`,
    // which is the selection — the accent washed over the scheme's background, `#D5E5E9` in
    // the light scheme — so the frames differ there and the dark one is the darker.
    let px = |n: &str| {
        let f = &frames.iter().find(|(name, _)| *name == n).expect("a frame").1;
        Framebuffer::get_pixel(f, 60, 4).expect("in bounds")
    };
    let (light, dark) = (px("ui"), px("ui-dark"));
    assert!(
        u32::from(dark.r) + u32::from(dark.g) + u32::from(dark.b)
            < u32::from(light.r) + u32::from(light.g) + u32::from(light.b),
        "the dark frame is not darker where the menu bar is: {dark:?} against {light:?}"
    );

    for (name, frame) in &frames {
        let (w, h, rgb) = rgb_of(frame);
        let png = encode_png(w, h, &rgb).expect("encode");
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().expect("a readable PNG");
        let info = reader.info().clone();
        assert_eq!((info.width, info.height), (w, h), "{name}: size on the way out");
        assert_eq!(info.color_type, png::ColorType::Rgb, "{name}: eight-bit RGB");

        let mut buf = vec![0u8; reader.output_buffer_size().expect("a bounded image")];
        let out = reader.next_frame(&mut buf).expect("one frame");
        let decoded = &buf[..out.buffer_size()];

        for y in 0..h {
            for x in 0..w {
                let want = Framebuffer::get_pixel(frame, x, y).unwrap_or_default();
                let i = ((y as usize) * (w as usize) + x as usize) * 3;
                assert_eq!(
                    (decoded[i], decoded[i + 1], decoded[i + 2]),
                    (want.r, want.g, want.b),
                    "{name}: pixel {x},{y} came back different"
                );
            }
        }
    }
}

/// The line scanner both directions share.
///
/// It exists because they did not share one: the code side stopped at the first marker
/// per line, so two markers on one line meant the second was reported missing while the
/// error pointed at the line carrying it. The bracketed-placeholder case is not
/// hypothetical either — this file's own prose contains one, and the first-marker-only
/// behaviour was hiding a *bare* marker written after it on the same line.
#[test]
fn markers_in_line_finds_every_plain_word_marker_and_no_others() {
    use super::markers_in_line;

    // Assembled, for the reason the neighbouring test gives.
    fn m(name: &str) -> String {
        format!("TODO{}{}{}", '(', name, ')')
    }

    assert_eq!(markers_in_line("nothing here"), Vec::<&str>::new());
    assert_eq!(markers_in_line(&format!("// {}", m("alpha"))), ["alpha"]);

    // Two on one line — the case that produced a false failure.
    assert_eq!(markers_in_line(&format!("// {} and {}: see the doc.", m("alpha"), m("beta"))), ["alpha", "beta"]);

    // A bracketed placeholder is skipped and does not stop the scan: a real marker
    // after it on the same line is still found.
    assert_eq!(markers_in_line(&format!("// spelt {} not {}", m("<tag>"), m("tag"))), ["tag"]);

    // Not markers.
    assert_eq!(markers_in_line(&format!("// {}", m("has space"))), Vec::<&str>::new());
    assert_eq!(markers_in_line(&format!("// {}", m(""))), Vec::<&str>::new());
    assert_eq!(markers_in_line("// TODO(unclosed"), Vec::<&str>::new());
}

/// The doc-to-code direction's extractor, on the shapes that decide what it enforces.
///
/// The Resolved cases are the reason this is a table test and not a live-document check:
/// a first pass at this measurement scraped the whole file and counted a tag named inside
/// a Resolved row — narrating the *deletion* of its own markers — as an unbacked entry.
/// Acting on that would have reinstated the stale marker that closing it removed.
///
/// The exemption case has no user in the document today, so this is the only thing
/// exercising it. An escape hatch nobody has opened is exactly the sort of thing that
/// turns out not to work the first time someone needs it.
///
/// **The fixture assembles its markers rather than spelling them.** `check-deferrals`
/// scans `tools/xtask/src`, so a literal `TODO` + `(name)` in this file *is* a marker as
/// far as the gate is concerned: writing the fixture out longhand made the gate fail on
/// its own test data, reporting seven deferrals that do not exist. Same trap as the
/// `TODO(<tag>)` placeholder in this module's prose, one level further in.
#[test]
fn open_section_tags_counts_open_entries_and_nothing_else() {
    use super::open_section_tags;

    // Assembled at runtime; see the note above.
    fn m(name: &str) -> String {
        format!("TODO{}{}{}", '(', name, ')')
    }

    let doc = format!(
        "# Deferred\n\
             **Thing one — `{alpha}`.** Words.\n\
             **Thing two — `{beta}`.** More words.\n\
             **Thing three — `{gamma}`.** No code site yet. {ncs}\n\
             **Two on one line — `{delta}` and `{epsilon}`.**\n\
             **Repeat — `{alpha}` again.**\n\
             **Cross-reference — also the trigger for `{theta}`.**\n\
             **The real one — `{theta}`.** No code site. {ncs}\n\
             **Not a tag — `{spaced}` and `{empty}`.**\n\
             \n## Resolved (kept for the record)\n\
             | Thing four (`{zeta}`) | closing it deleted its markers |\n\
             \n## How to use this document\n\
             Every `{eta}` must appear here.\n",
        alpha = m("alpha"),
        beta = m("beta"),
        gamma = m("gamma"),
        delta = m("delta"),
        epsilon = m("epsilon"),
        theta = m("theta"),
        spaced = m("has space"),
        empty = m(""),
        zeta = m("zeta"),
        eta = m("eta"),
        ncs = NO_CODE_SITE,
    );

    let got = open_section_tags(&doc);
    let names: Vec<&str> = got.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["alpha", "beta", "gamma", "delta", "epsilon", "theta"]);

    // Only the marked line is exempt.
    let exempt: Vec<&str> = got.iter().filter(|(_, e)| *e).map(|(t, _)| t.as_str()).collect();
    // `theta` is named on an earlier line as a cross-reference and exempted on its own
    // line below — the geometry `tty-server` -> `session-metadata-server` already has in
    // the real document. First-occurrence-wins drops the exemption and fails the gate
    // while telling you to write the marker you just wrote.
    assert_eq!(exempt, ["gamma", "theta"], "the no-code-site marker must be honoured wherever in the entry it appears");

    // The three that must NOT appear, and the reason each is excluded.
    assert!(!names.contains(&"zeta"), "a Resolved row's tag is not an open entry");
    assert!(!names.contains(&"eta"), "the closing section's prose is not an entry");
    assert!(!names.contains(&"has space"), "a tag must be a plain word");

    // A document with no Resolved heading is open all the way down.
    let all_open = open_section_tags(&format!("**One — `{}`.**\n", m("solo")));
    assert_eq!(all_open, vec![(String::from("solo"), false)]);
}

/// The echo guard's predicate, on the shapes it has to tell apart.
///
/// The flagship pair is the last one: the exact command `test-interactive` step 14 sends,
/// against the pattern that shipped for months (`boom` — satisfied by the echo, so the
/// step passed with `try`/`catch` deleted) and the one that replaced it (`caught=boom` —
/// produced only by evaluating the catch block). A predicate that cannot separate those
/// two is decoration.
#[test]
fn echo_source_finds_a_pattern_the_guest_would_echo_back() {
    use super::echo_source;

    let none: [String; 0] = [];
    // Gates that type over QMP send nothing, so the guard must stay inert for them.
    assert_eq!(echo_source(&none, "anything"), None);

    let one = [String::from("format(\"add={}\", sum)")];
    assert_eq!(echo_source(&one, "add=5"), None, "the answer is not in the command");
    assert_eq!(echo_source(&one, "sum"), Some("format(\"add={}\", sum)"));

    // An empty pattern is contained in every string; it must not trip the guard.
    assert_eq!(echo_source(&one, ""), None);

    // **Two sends before one expect.** Both echoes are still ahead of the cursor, so the
    // earlier one has to be caught too — a predicate holding only the latest send would
    // return None here and wave the assertion through.
    let two = [String::from("echo hello"), String::from("whoami")];
    assert_eq!(echo_source(&two, "hello"), Some("echo hello"));
    assert_eq!(echo_source(&two, "whoami"), Some("whoami"));
    assert_eq!(echo_source(&two, "alice"), None, "the answer is in neither command");

    // The regression, both directions.
    let step14 = [String::from("try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }")];
    assert!(echo_source(&step14, "boom").is_some(), "the old pattern must be refused");
    assert_eq!(echo_source(&step14, "caught=boom"), None, "the new one must be allowed");
}

/// The rule-2 classifier, on every form it has to tell apart.
///
/// Table-tested because it *is* the fix: before this, the only thing proving the
/// classifier worked was a manual mutation of `idt.rs`, which is exactly the situation
/// the gate argues against. `Discarded` is the case that shipped green for months —
/// `let _ =` keeps `#[must_use]` quiet and closes the scope before the handler body.
#[test]
fn scope_binding_tells_a_held_guard_from_one_dropped_where_it_is_made() {
    use super::{ScopeBinding, scope_binding};

    // Held: any real binding, however it is spelt.
    for line in [
        "            let _lock_scope = crate::libkern::lockrank::enter_interrupt();",
        "let g = enter_interrupt();",
        "let mut g = enter_interrupt();",
        "let g: IrqScope = enter_interrupt();",
        "let mut g: IrqScope = enter_interrupt();",
    ] {
        assert_eq!(scope_binding(line), ScopeBinding::Held, "{line}");
    }

    // Discarded: the underscore *pattern*, not a name that begins with one.
    assert_eq!(scope_binding("let _ = enter_interrupt();"), ScopeBinding::Discarded);
    assert_eq!(scope_binding("  let _   =   enter_interrupt();"), ScopeBinding::Discarded);
    assert_eq!(scope_binding("let _x = enter_interrupt();"), ScopeBinding::Held);

    // Temporary: no binding at all, so it drops at the end of the statement.
    assert_eq!(scope_binding("enter_interrupt();"), ScopeBinding::Temporary);
    assert_eq!(scope_binding("        enter_interrupt();"), ScopeBinding::Temporary);
    // A comment mentioning it is not a call site either.
    assert_eq!(scope_binding("/// calls enter_interrupt()"), ScopeBinding::Temporary);
}

use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Per-test unique tmp dir. We avoid `tempfile` to keep xtask
/// dependency-free, so we have to clean up manually.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("nitrox-xtask-{}-{}-{}", tag, std::process::id(), n));
        if path.exists() {
            fs::remove_dir_all(&path).expect("clear stale tmp");
        }
        fs::create_dir_all(&path).expect("create tmp");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn touch(p: &Path) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).expect("mkdir -p");
    }
    fs::write(p, b"").expect("touch");
}

#[test]
fn walk_for_finds_nested_file() {
    let tmp = TmpDir::new("walk-nested");
    touch(&tmp.path().join("a/b/c/target.bin"));
    let found = walk_for(tmp.path(), "target.bin").unwrap();
    let found = found.expect("walk_for should locate target.bin");
    assert_eq!(found.file_name().unwrap(), "target.bin");
}

#[test]
fn walk_for_returns_none_when_missing() {
    let tmp = TmpDir::new("walk-missing");
    fs::create_dir_all(tmp.path().join("a")).unwrap();
    assert!(walk_for(tmp.path(), "nope.efi").unwrap().is_none());
}

#[test]
fn find_bootx64_uses_known_location() {
    let tmp = TmpDir::new("bootx64-known");
    // Limine v12 layout: efi/x86_64/BOOTX64.EFI
    let expected = tmp.path().join("efi/x86_64/BOOTX64.EFI");
    touch(&expected);
    // Decoy that should be ignored because the known location wins.
    touch(&tmp.path().join("somewhere/else/BOOTX64.EFI"));
    let found = find_bootx64(tmp.path()).unwrap();
    assert_eq!(found, expected);
}

#[test]
fn find_bootx64_falls_back_to_recursive_scan() {
    let tmp = TmpDir::new("bootx64-fallback");
    let weird = tmp.path().join("unexpected/depth/BOOTX64.EFI");
    touch(&weird);
    let found = find_bootx64(tmp.path()).unwrap();
    assert!(found.ends_with("BOOTX64.EFI"));
}

#[test]
fn find_bootx64_errors_when_absent() {
    let tmp = TmpDir::new("bootx64-absent");
    fs::create_dir_all(tmp.path().join("efi")).unwrap();
    assert!(find_bootx64(tmp.path()).is_err());
}

#[test]
fn format_cmd_includes_program_and_args() {
    let mut cmd = Command::new("echo");
    cmd.arg("hello").arg("world");
    assert_eq!(format_cmd(&cmd), "echo hello world");
}

#[test]
fn format_cmd_handles_no_args() {
    let cmd = Command::new("true");
    assert_eq!(format_cmd(&cmd), "true");
}

#[test]
fn parse_host_extracts_linux_triple() {
    let sample = "\
rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2
";
    assert_eq!(parse_host_from_rustc_vv(sample).as_deref(), Some("x86_64-unknown-linux-gnu"));
}

#[test]
fn parse_host_extracts_macos_triple() {
    let sample = "rustc 1.95.0\nhost: aarch64-apple-darwin\n";
    assert_eq!(parse_host_from_rustc_vv(sample).as_deref(), Some("aarch64-apple-darwin"));
}

#[test]
fn parse_host_returns_none_when_absent() {
    let sample = "rustc 1.95.0\nrelease: 1.95.0\n";
    assert!(parse_host_from_rustc_vv(sample).is_none());
}

/// **A test root may differ in the two data files and its own package, and nothing else**
/// (administration Part E.1c) — including the control the plan names: a file added to one
/// root, on either side.
#[test]
fn a_test_root_differs_from_a_release_one_only_where_allowed() {
    use std::collections::BTreeMap;
    let file = |n: u64| TreeEntry::File { size: n, hash: n };
    let pkg = "store/abc-test-0.1.0";
    let release: BTreeMap<String, TreeEntry> = [
        ("system", TreeEntry::Dir),
        ("system/services.toml", file(1)),
        ("system/profiles/system.toml", file(2)),
        ("store/def-system-0.1.0/bin/service-mgr", file(3)),
        ("store/0a1-coreutils-0.1.0/bin/list", file(8)),
        ("store/0a1-coreutils-0.1.0/bin/nxterm", file(9)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let mut test: BTreeMap<String, TreeEntry> = [
        ("system", TreeEntry::Dir),
        ("system/services.toml", file(10)),
        ("system/profiles/system.toml", file(20)),
        ("store/def-system-0.1.0/bin/service-mgr", file(3)),
        // The same package under another hash, since its `nxterm` is the test-harness build.
        ("store/fe2-coreutils-0.1.0/bin/list", file(8)),
        ("store/fe2-coreutils-0.1.0/bin/nxterm", file(90)),
        (pkg, TreeEntry::Dir),
        ("store/abc-test-0.1.0/bin/boot-probe", file(4)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(root_divergence(&release, &test, pkg), Vec::<String>::new());
    // A file added to the test root, outside its package.
    test.insert("system/extra".into(), file(5));
    assert_eq!(root_divergence(&release, &test, pkg), ["system/extra: only on the test root"]);
    test.remove("system/extra");
    // A program that differs.
    test.insert("store/def-system-0.1.0/bin/service-mgr".into(), file(6));
    assert_eq!(root_divergence(&release, &test, pkg), ["store/system-0.1.0/bin/service-mgr: differs"]);
    test.insert("store/def-system-0.1.0/bin/service-mgr".into(), file(3));
    // Another program in the package `nxterm` is in, differing too: only `nxterm` may.
    test.insert("store/fe2-coreutils-0.1.0/bin/list".into(), file(80));
    assert_eq!(root_divergence(&release, &test, pkg), ["store/coreutils-0.1.0/bin/list: differs"]);
    test.insert("store/fe2-coreutils-0.1.0/bin/list".into(), file(8));
    // A file added to the release root, and the test package on a release root.
    let mut release2 = release;
    release2.insert("system/extra".into(), file(5));
    release2.insert("store/abc-test-0.1.0/bin/boot-probe".into(), file(4));
    let mut got = root_divergence(&release2, &test, pkg);
    got.sort();
    assert_eq!(
        got,
        [
            "store/test-0.1.0/bin/boot-probe: the test package, on a release root",
            "system/extra: only on the release root",
        ]
    );
    assert_eq!(unhashed("store/0a1-coreutils-0.1.0/bin/x"), "store/coreutils-0.1.0/bin/x");
    assert_eq!(unhashed("store/not-hex-0.1.0"), "store/not-hex-0.1.0", "no hash to take out");
    assert_eq!(unhashed("system/users"), "system/users");
    // A name that only begins like the package is not in it.
    test.insert("store/abc-test-0.1.0x".into(), file(7));
    assert_eq!(root_divergence(&release2, &test, pkg).len(), 3);
}

/// The declarations' names in file order, each with whether it says `critical = true`.
fn declared(text: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("[service.").and_then(|r| r.strip_suffix(']')) {
            if !name.contains('.') {
                out.push((name.to_string(), false));
            }
        } else if line == "critical = true" {
            out.last_mut().expect("a key before any table").1 = true;
        }
    }
    out
}

/// **The emergency shell and the terminal server never hold the console together**
/// (`console-and-tty.md` § *`eshell` is separate*). `service-mgr` asks `init` for the shell
/// only when a `critical` server does not come up at boot, so the invariant is this file's
/// order: every critical server is declared before `tty-server`, and so has failed, if it
/// fails, before anything holds the console. Checked in every image's declarations.
#[test]
fn every_critical_server_is_declared_before_the_terminal_server() {
    // **What ships, from the one function that builds it** (PR #340 review, finding 7): a copy
    // assembled here missed `HEARTBEAT_TOML` the day E.1c put it between the two.
    let images = [
        ("release", services_toml(BuildMode::Normal)),
        ("test", services_toml(BuildMode::TestHarness)),
        ("bench", services_toml(BuildMode::Bench)),
    ];
    for (image, text) in &images {
        let decls = declared(text);
        let tty = decls.iter().position(|(n, _)| n == "tty-server").expect("tty-server is declared");
        let critical: Vec<usize> = (0..decls.len()).filter(|&i| decls[i].1).collect();
        assert!(!critical.is_empty(), "{image}: no critical server is declared at all");
        for i in critical {
            assert!(i < tty, "{image}: '{}' is critical and declared after tty-server", decls[i].0);
        }
    }
    // The scan reads what it is aimed at: a critical declaration moved past the terminal
    // server is found.
    let moved = format!("{SERVICES_TOML}[service.late]\nexecutable = \"/bin/late\"\ncritical = true\n");
    let decls = declared(&moved);
    assert_eq!(decls.last(), Some(&("late".to_string(), true)));
}
