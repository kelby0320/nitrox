//! More of xtask's unit tests: the gates' parsers, the live menu, QMP, diagnosis.

use super::*;

/// The listing counts only after the start — the check `check-login`'s step 13 makes, and the
/// one a whole-transcript search could not fail (PR #314 review, blocking 2).
#[test]
fn a_browser_started_at_a_place_must_list_it_afterwards() {
    let p = "/home/Documents";
    // The sidebar step's listing, then a browser that started there and went home.
    let went_home = "nxfiles: listed /home/Documents - 2 entries\r\n\
                         nxfiles: starting at /home/Documents\r\n\
                         nxfiles: listed /home - 5 entries\r\n";
    assert!(started_and_listed(went_home, p).is_err(), "the earlier listing must not count");
    let honest = "nxfiles: listed /home/Documents - 2 entries\n\
                      nxfiles: starting at /home/Documents\n\
                      desktop-shell: placed window 20 at 24,54\n\
                      nxfiles: listed /home/Documents - 2 entries\n";
    assert_eq!(started_and_listed(honest, p), Ok(()));
    // A deeper directory is not this one, at either end.
    let deeper = "nxfiles: starting at /home/Documents/sub\nnxfiles: listed /home/Documents - 1 entries\n";
    assert!(started_and_listed(deeper, p).is_err());
    assert!(started_and_listed("nxfiles: listed /home/Documents - 1 entries\n", p).is_err());
}

#[test]
fn the_derived_click_points_at_the_old_size_are_the_literals_they_replaced() {
    // The gates aimed at these by hand at 1280x800; deriving them from the size must give the
    // same answer there, or the move to 1360x768 changed what a gate clicks and not just where.
    //
    // **Except by the bar's height**, which the desktop refresh's Part C changed on purpose:
    // the bars are 30 pixels since, so every aim at the bottom bar is six higher than the
    // literal it replaced (776 and 788 were 24-pixel answers).
    let old = DisplaySize::parse("1280x800").unwrap();
    assert_eq!(old.corner(), (1279, 799));
    assert_eq!(old.centre(), (640, 400));
    assert_eq!((old.bottom_bar_y(), old.bottom_bar_click_y()), (770, 785));
    assert_eq!(old.desktop_name_click(), (1250, 785));
    assert_eq!((old.task_click(0), old.task_click(1), old.show_desktop_click()), ((141, 785), (331, 785), (20, 785)));
    assert_eq!(old.card_w(), 293, "the design's 330 on 1440, at this width");
    assert_eq!(old.pin_motions(), 20, "the pin the gates always used at this size");
    let big = DisplaySize::parse("2560x1440").unwrap();
    assert!(big.pin_motions() * 100 > big.w as usize, "a pin that cannot cross the screen");
    let gate = DisplaySize::GATE;
    assert_eq!((gate.bottom_bar_y(), gate.desktop_name_click()), (738, (1330, 753)));
    assert_eq!((gate.card_w(), gate.card_cols()), (311, 3));
}

#[test]
fn a_width_qemu_would_shear_is_refused_and_says_what_it_would_show() {
    // The number is computed, so it is asserted where it sits in the sentence: the message
    // also names 1360 in its closing clause, which a bare `contains("1360")` would match
    // whatever width was computed. 1370 is a second width whose answer appears nowhere else.
    for (text, scanned, drawn) in [("1366x768", 1360, 1366), ("1370x768", 1368, 1370)] {
        let e = DisplaySize::parse(text).unwrap_err().to_string();
        let says = format!("scan out {scanned} while the guest draws {drawn}");
        assert!(e.contains(&says), "the refusal of {text} names what QEMU would scan out: {e}");
    }
    assert!(DisplaySize::parse("1360x768").is_ok(), "control: a multiple of 8 is taken");
    for bad in ["1360", "x768", "1360x", "wide", "13 60x768", "320x200", "8192x768"] {
        assert!(DisplaySize::parse(bad).is_err(), "{bad} was accepted");
    }
}

#[test]
fn the_live_menu_boots_the_release_entry_by_default_and_offers_the_report_and_the_installer() {
    let base = fs::read_to_string(limine_conf()).expect("boot/limine.conf is readable");
    let conf = live_limine_conf(&base).expect("the shipped limine.conf is one entry");
    assert_eq!(
        conf,
        "timeout: 5\n\
             \n\
             /Nitrox\n\
             \x20   protocol: limine\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \n\
             /Nitrox — hardware report\n\
             \x20   protocol: limine\n\
             \x20   cmdline: hwreport\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \n\
             /Nitrox — install to this machine\n\
             \x20   protocol: limine\n\
             \x20   path: boot():/boot/kernel\n\
             \x20   module_path: boot():/boot/initramfs\n\
             \x20   module_path: boot():/boot/root.img\n\
             \x20   module_path: boot():/boot/install-esp.img\n\
             \x20   module_path: boot():/boot/install-root.img\n"
    );
    // **No entry says `install`** (administration Part G.3): nothing reads it now, and a
    // session is ordinary whichever entry booted. The report entry's `hwreport` is the only
    // command line.
    let first = conf.split("\n\n").nth(1).expect("the default entry");
    assert!(!first.contains("cmdline:"), "the default entry must pass nothing: {first:?}");
    assert_eq!(conf.matches("cmdline:").count(), 1, "hwreport alone: {conf}");
    // **And carries no installable ESP**, which is what keeps it as cheap to boot as it was:
    // the module is 33 MiB of firmware reads and of permanently-held RAM, and an ordinary
    // session could not reach the device it becomes anyway.
    assert_eq!(
        conf.matches("install-esp.img").count(),
        1,
        "only the installer entry loads the installable ESP: {conf}"
    );
    // **Nor the pristine root** (administration Part G.1), for the same reason.
    assert_eq!(conf.matches("install-root.img").count(), 1, "only the installer entry loads the pristine root: {conf}");
}

#[test]
fn the_live_menu_refuses_a_base_it_would_have_to_guess_about() {
    let one = "timeout: 0\n\n/Nitrox\n    protocol: limine\n    path: boot():/boot/kernel\n    module_path: boot():/boot/initramfs\n";
    assert!(live_limine_conf(one).is_ok(), "control: the one-entry shape is accepted");
    assert!(live_limine_conf(&one.replace("timeout: 0", "timeout: 3")).is_err());
    assert!(live_limine_conf(&format!("{one}\n/Other\n    protocol: limine\n")).is_err());
    assert!(live_limine_conf(&one.replace("    module_path: boot():/boot/initramfs\n", "")).is_err());
}

/// **The scratch module lands in the one entry, after the initramfs**, and the shipped conf is
/// the shape that allows it. The kernel publishes every module after the first as a RAM disk,
/// so the order is what keeps the initramfs the initramfs.
#[test]
fn a_test_conf_loads_the_scratch_disk_after_the_initramfs() {
    let base = fs::read_to_string(limine_conf()).expect("boot/limine.conf reads");
    let conf = test_limine_conf(&base).expect("the shipped limine.conf is one entry");
    let initramfs = conf.find("boot():/boot/initramfs").expect("the initramfs is loaded");
    let scratch = conf.find("boot():/boot/scratch.img").expect("the scratch disk is loaded");
    assert!(initramfs < scratch, "the initramfs is module 0: {conf}");
    assert_eq!(conf.matches("module_path:").count(), 2);
    let one = "timeout: 0\n\n/Nitrox\n    protocol: limine\n    path: boot():/boot/kernel\n    module_path: boot():/boot/initramfs\n";
    assert!(test_limine_conf(one).is_ok(), "control: the one-entry shape is accepted");
    assert!(test_limine_conf(&format!("{one}\n/Other\n    protocol: limine\n")).is_err());
    assert!(test_limine_conf(&one.replace("    module_path: boot():/boot/initramfs\n", "")).is_err());
}

#[test]
fn a_qmp_return_string_is_unescaped() {
    assert_eq!(unescape_json_return(r#"{"return": "a\nb"}"#), "a\nb");
    assert_eq!(unescape_json_return(r#"{"return": "he said \"hi\""}"#), "he said \"hi\"");
    assert_eq!(unescape_json_return(r#"{"return": "C:\\x"}"#), "C:\\x");
    assert_eq!(unescape_json_return(r#"{"return": ""}"#), "", "an empty monitor reply");
}

#[test]
fn a_reply_without_a_return_field_yields_nothing_rather_than_garbage() {
    assert_eq!(unescape_json_return(r#"{"error": {"class": "GenericError"}}"#), "");
    assert_eq!(unescape_json_return("not json at all"), "");
}

#[test]
fn demangling_recovers_a_rust_path() {
    assert_eq!(demangle("_ZN13nitrox_kernel5sched9idle_body17h08d8be4e35d6ac8aE"), "nitrox_kernel::sched::idle_body");
}

#[test]
fn demangling_drops_the_llvm_suffix_and_the_hash() {
    assert_eq!(
        demangle("_ZN13nitrox_kernel7drivers4ahci12issue_locked17h0ddde3632e042bdcE.llvm.29"),
        "nitrox_kernel::drivers::ahci::issue_locked"
    );
}

#[test]
fn an_unmangled_or_v0_name_comes_back_unchanged() {
    // The failure this guards: a future rustc switches the kernel to v0 mangling and
    // `demangle` silently passes everything through, leaving the next hang dump full of
    // `_RNvNt…`. Passing through is the right behaviour — printing *something* beats
    // printing nothing — but it must be a decision, not an accident.
    assert_eq!(demangle("memcpy"), "memcpy");
    assert_eq!(demangle("_RNvNtCs1234_4core3fmt5write"), "_RNvNtCs1234_4core3fmt5write");
    assert_eq!(demangle("_ZN"), "_ZN", "truncated: not the shape we expect");
    assert_eq!(demangle("_ZNxyz"), "_ZNxyz", "no length prefix");
}

#[test]
fn a_symbol_is_resolved_only_inside_its_extent() {
    let syms =
        vec![ElfSym { addr: 0x1000, size: 0x40, name: "a".into() }, ElfSym { addr: 0x2000, size: 0, name: "b".into() }];
    assert_eq!(resolve_symbol(&syms, 0x1000).as_deref(), Some("a+0x0"));
    assert_eq!(resolve_symbol(&syms, 0x1020).as_deref(), Some("a+0x20"));
    // Past `a`'s extent but before `b`: unknown, rather than attributed to `a`. A dump
    // that names the wrong function is worse than one that admits it does not know.
    assert_eq!(resolve_symbol(&syms, 0x1040), None);
    assert_eq!(resolve_symbol(&syms, 0x0), None, "below everything");
    // A zero-size symbol still names its entry point.
    assert_eq!(resolve_symbol(&syms, 0x2000).as_deref(), Some("b+0x0"));
    assert_eq!(resolve_symbol(&syms, 0x9999).as_deref(), Some("b+0x7999"));
}

/// A minimal ELF64 with one section header table, one `SHT_SYMTAB` and its strtab,
/// carrying two `STT_FUNC` symbols and one non-func that must be ignored.
fn tiny_elf() -> Vec<u8> {
    const SHOFF: usize = 0x100;
    const SHENT: usize = 64;
    let strtab: &[u8] = b"\0alpha\0beta\0notafunc\0";
    let stroff = 0x300usize;
    let symoff = 0x200usize;
    let mut b = vec![0u8; 0x400];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // ELF64
    b[0x28..0x30].copy_from_slice(&(SHOFF as u64).to_le_bytes());
    b[0x3A..0x3C].copy_from_slice(&(SHENT as u16).to_le_bytes());
    b[0x3C..0x3E].copy_from_slice(&2u16.to_le_bytes()); // two section headers

    // Section 0: the symtab. Section 1: its string table.
    let sh = SHOFF;
    b[sh + 4..sh + 8].copy_from_slice(&2u32.to_le_bytes()); // SHT_SYMTAB
    b[sh + 0x18..sh + 0x20].copy_from_slice(&(symoff as u64).to_le_bytes());
    b[sh + 0x20..sh + 0x28].copy_from_slice(&(24u64 * 3).to_le_bytes());
    b[sh + 0x28..sh + 0x2C].copy_from_slice(&1u32.to_le_bytes()); // sh_link -> strtab
    let sh1 = SHOFF + SHENT;
    b[sh1 + 4..sh1 + 8].copy_from_slice(&3u32.to_le_bytes()); // SHT_STRTAB
    b[sh1 + 0x18..sh1 + 0x20].copy_from_slice(&(stroff as u64).to_le_bytes());

    b[stroff..stroff + strtab.len()].copy_from_slice(strtab);

    let mut put_sym = |i: usize, nameoff: u32, info: u8, addr: u64, size: u64| {
        let e = symoff + i * 24;
        b[e..e + 4].copy_from_slice(&nameoff.to_le_bytes());
        b[e + 4] = info;
        b[e + 8..e + 16].copy_from_slice(&addr.to_le_bytes());
        b[e + 16..e + 24].copy_from_slice(&size.to_le_bytes());
    };
    put_sym(0, 1, 2, 0x2000, 0x10); // "alpha", STT_FUNC — deliberately out of order
    put_sym(1, 7, 2, 0x1000, 0x20); // "beta",  STT_FUNC
    put_sym(2, 12, 1, 0x3000, 0x8); // "notafunc", STT_OBJECT — must be skipped
    b
}

#[test]
fn the_elf_walker_reads_func_symbols_sorted_and_skips_the_rest() {
    let dir = std::env::temp_dir().join("nitrox-xtask-elf-test");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("tiny.elf");
    fs::write(&path, tiny_elf()).unwrap();
    let syms = elf_function_symbols(&path).unwrap();
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["beta", "alpha"], "sorted by address, and only STT_FUNC");
    assert_eq!(syms[0].addr, 0x1000);
    assert_eq!(syms[1].size, 0x10);
    // And the resolver agrees with the walker's ordering.
    assert_eq!(resolve_symbol(&syms, 0x2008).as_deref(), Some("alpha+0x8"));
    let _ = fs::remove_file(&path);
}

#[test]
fn a_non_elf_is_refused_rather_than_misparsed() {
    let dir = std::env::temp_dir().join("nitrox-xtask-elf-test");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("not.elf");
    fs::write(&path, b"#!/bin/sh\necho hi\n").unwrap();
    assert!(elf_function_symbols(&path).is_err());
    let _ = fs::remove_file(&path);
}

/// The terminal's ground is its own, and no cell colour is invisible on it.
///
/// **What this used to claim, and why each version was replaced.** M11 Part B first asserted
/// that no ANSI colour equals a *chrome* colour — an attempt to encode "a theme must not
/// retint what programs print" as an inequality. It is the wrong encoding, and the palette
/// contained the counterexample: `ansi[0]` and `title_inactive` were both `#1C222A`, chosen
/// independently as the darkest tone in one scheme. It passed only because `title_inactive`
/// was the one theme colour missing from its list (PR #262 review, blocking 1). **Provenance
/// is not equality**, and a test comparing values cannot see provenance.
///
/// Its replacement asserted that the grid's two defaults *followed* the theme, which was
/// structural from Part B until Part E cut the tie. What is asserted now is the decision that
/// replaced it, and the evidence for it:
///
/// - **The grid's ground is not the desktop's**, which fails if somebody re-ties them.
/// - **No cell colour is the ground it is drawn on.** A cell painted in the background colour
///   is text that cannot be read at all — the one legibility property with a sharp edge.
///   (Nearly-equal is a judgement; ANSI black on a dark ground is dim everywhere, which is
///   the convention rather than a bug.)
/// - **And the reason the tie was cut, as a live check**: the brightest of the sixteen is
///   within a hair of the desktop's white. Following the theme would put invisible text on
///   screen, and avoiding that would mean retuning the sixteen — the one thing the rule above
///   forbids. If somebody ever does retune them for a light ground, this fails and says so,
///   which is the moment to revisit the decision rather than to delete the assertion.
#[test]
fn the_terminals_ground_is_its_own_and_no_cell_is_invisible_on_it() {
    let theme = libui::paint::Theme::default();
    let palette = libterm::cell::Palette::default();

    assert_ne!(
        palette.background, theme.background,
        "the grid's ground was re-tied to the desktop's (M11 Part E); see the assertion below"
    );

    for (i, c) in palette.ansi.iter().enumerate() {
        assert_ne!(
            *c, palette.background,
            "ANSI colour {i} is the terminal's own background — text in it is invisible"
        );
    }

    let gap = |a: libdraw::format::Rgb, b: libdraw::format::Rgb| {
        (a.r.abs_diff(b.r)).max(a.g.abs_diff(b.g)).max(a.b.abs_diff(b.b))
    };
    let brightest = palette
        .ansi
        .iter()
        .copied()
        .max_by_key(|c| u32::from(c.r) + u32::from(c.g) + u32::from(c.b))
        .expect("sixteen colours");
    assert!(
        gap(brightest, theme.background) <= 32,
        "the brightest ANSI colour is {brightest:?}, no longer close to the desktop's ground \
             {:?} — the sixteen may have been retuned for a light ground, which is the trigger \
             for revisiting whether the grid should follow the theme after all",
        theme.background
    );
}

#[test]
fn a_dialog_placement_reads_back_as_six_numbers() {
    // The line `check-login` aims a click from. It carries the parent as well as the origin,
    // because the assertion beside it is that the dialog was centred *on that window* — a
    // parser that dropped the parent would leave the gate checking arithmetic against a
    // window it had not identified.
    assert_eq!(parse_dialog_placement(" 21 of window 20 at 490,214 340x132"), Some((21, 20, 490, 214, 340, 132)));
    // A negative origin is legal: the clamp keeps a dialog inside the work area, and the
    // work area's own origin is not the screen's.
    assert_eq!(parse_dialog_placement("3 of window 2 at -4,24 100x50"), Some((3, 2, -4, 24, 100, 50)));
    // And the shapes that must not silently parse as something else.
    assert_eq!(parse_dialog_placement("21 at 490,214 340x132"), None, "no parent");
    assert_eq!(parse_dialog_placement("21 of window 20 at 490 340x132"), None, "no comma");
    assert_eq!(parse_dialog_placement(""), None);
}

#[test]
fn a_taskbar_slot_is_the_position_of_the_id_not_the_id() {
    // **The distinction the gate depends on.** Ids are not slots — a window closed earlier
    // in the run leaves the ones after it at lower positions — and aiming by id would land
    // on somebody else's entry as soon as anything had ever been closed.
    let list = "Desktop 1 of 2 [20:nxfiles] [24:notes.txt*] [31:untitled]";
    assert_eq!(taskbar_slot(list, 20), Some(0));
    assert_eq!(taskbar_slot(list, 24), Some(1));
    assert_eq!(taskbar_slot(list, 31), Some(2));
    assert_eq!(taskbar_slot(list, 21), None, "a window on another desktop has no slot");
    // A title that contains a digit must not be mistaken for an id: the match is on the
    // group's own `id:` prefix, not on the group containing the number anywhere.
    assert_eq!(taskbar_slot("Desktop 1 of 1 [7:file20.txt]", 20), None);
    assert_eq!(taskbar_slot("Desktop 1 of 1 (empty)", 20), None);
}

/// A QMP server stand-in on a fresh socket: it writes `opening`, then answers one command
/// with an empty `return`, as QEMU answers `qmp_capabilities`.
fn fake_qmp(opening: String) -> std::path::PathBuf {
    use std::io::{BufRead as _, Write as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("xtask-qmp-{}-{n}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.write_all(opening.as_bytes()).unwrap();
        let mut command = String::new();
        let _ = std::io::BufReader::new(s.try_clone().unwrap()).read_line(&mut command);
        let _ = s.write_all(b"{\"return\": {}}\n");
    });
    path
}

/// **An event before the greeting is skipped, not taken for it** — CI's Display gate on
/// 2026-09-23, whose first line was a `RESUME` event. At the bound's neighbour: as many
/// events as it allows still connect, and one more fails as a socket that never greets.
#[test]
fn qmp_connect_reads_past_events_to_the_greeting() {
    const GREETING: &str = "{\"QMP\": {\"version\": {}, \"capabilities\": []}}\n";
    const RESUME: &str = "{\"timestamp\": {\"seconds\": 1, \"microseconds\": 2}, \"event\": \"RESUME\"}\n";
    let connect = |events: usize| super::Qmp::connect(&fake_qmp(format!("{}{GREETING}", RESUME.repeat(events))));
    assert!(connect(0).is_ok(), "the ordinary order");
    assert!(connect(1).is_ok(), "an event first");
    assert!(connect(super::MAX_EVENTS_BEFORE_GREETING).is_ok(), "as many as allowed");
    let e = connect(super::MAX_EVENTS_BEFORE_GREETING + 1).err().expect("one too many");
    assert!(e.to_string().contains("unexpected QMP greeting"), "{e}");
}

/// **A character split across two reads arrives whole** — the failure that made
/// `test-interactive` wait for `sleep — exited` with `sleep �� exited` in its transcript.
#[test]
fn a_character_split_across_reads_is_decoded_whole() {
    let line = "sleep — exited\n".as_bytes();
    let dash = line.iter().position(|&b| b == 0xE2).unwrap();
    let mut pending = Vec::new();
    pending.extend_from_slice(&line[..dash + 1]); // the read ends one byte into the dash
    let first = super::take_utf8(&mut pending);
    assert_eq!(first, "sleep ");
    assert_eq!(pending, [0xE2], "the dash's first byte waits for the rest");
    pending.extend_from_slice(&line[dash + 1..]);
    let second = super::take_utf8(&mut pending);
    assert_eq!(format!("{first}{second}"), "sleep — exited\n");
    assert!(pending.is_empty());
    // Bytes that are not UTF-8 at all still decode, lossily, rather than being held forever.
    let mut bad = vec![b'a', 0xFF, b'b'];
    assert_eq!(super::take_utf8(&mut bad), "a\u{FFFD}b");
    assert!(bad.is_empty());
}
/// **The last window list is a whole line's** (Phase 6 Part B.7): a list cut off as it arrived is
/// passed over for the whole one before it — the transcript `check-login --usb` kept when it
/// failed — and a transcript with no whole list has none.
#[test]
fn the_last_window_list_is_a_whole_lines() {
    let kept = "desktop-shell: placed window 24 at 24,150\n\
                    desktop-shell: window list on cli of 3 [21:  Files] [23:  notes.txt] [24:> window 24]\n\
                    nxedit: opened another window\n\
                    desktop-shell: window list on cli of 3 [21:  Files] [23:  notes.t";
    assert_eq!(last_window_list(kept), Some("3 [21:  Files] [23:  notes.txt] [24:> window 24]"));
    assert_eq!(last_window_list(kept).map(|l| l.matches('[').count()), Some(3));
    let ended = format!("{kept}xt] [24:  window 24]\n");
    assert_eq!(last_window_list(&ended), Some("3 [21:  Files] [23:  notes.txt] [24:  window 24]"));
    assert_eq!(last_window_list("desktop-shell: window list on cli of 3 [21:  Files]"), None);
}
