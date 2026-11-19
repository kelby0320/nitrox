use super::*;

#[test]
fn every_chord_is_its_own() {
    for (i, a) in CHORDS.iter().enumerate() {
        assert_ne!(a.id, 0, "{} has the id that registers nothing", a.keys());
        assert!(!a.keys().contains('?'), "chord {} has a key with no name", a.id);
        for b in &CHORDS[i + 1..] {
            assert_ne!(a.id, b.id, "{} and {} share an id", a.keys(), b.keys());
            assert_ne!((a.mods, a.code), (b.mods, b.code), "{} is registered twice", a.keys());
            assert_ne!(a.action, b.action, "{} and {} do the same thing", a.keys(), b.keys());
        }
        assert_eq!(action(a.id), Some(a.action), "{}'s id finds another chord", a.keys());
    }
    assert_eq!(action(0), None);
}

#[test]
fn the_number_chords_reach_each_desktop_by_its_own_key() {
    for n in 1..=CHORD_DESKTOPS {
        let switch = CHORDS.iter().find(|c| c.action == Action::Switch(n));
        let moves = CHORDS.iter().find(|c| c.action == Action::MoveTo(n));
        assert_eq!(switch.map(|c| c.keys()).as_deref(), Some(format!("Super+{n}").as_str()));
        assert_eq!(moves.map(|c| c.keys()).as_deref(), Some(format!("Super+Shift+{n}").as_str()));
    }
    let numbered = CHORDS.iter().filter(|c| matches!(c.action, Action::Switch(_) | Action::MoveTo(_)));
    assert_eq!(numbered.count(), 2 * CHORD_DESKTOPS);
}

#[test]
fn each_chord_registers_its_own_id_modifiers_and_key() {
    for c in CHORDS {
        assert_eq!(c.hotkey(), MgrHotkey { id: c.id, mods: c.mods, code: c.code }, "{}", c.keys());
    }
}

#[test]
fn a_chord_is_spelled_from_every_modifier_it_holds() {
    let chord = |mods| Chord { id: 9, mods, code: KEY_R, action: Action::Rename, does: "" }.keys();
    assert_eq!(chord(MOD_META | MOD_CTRL | MOD_ALT | MOD_SHIFT), "Super+Ctrl+Alt+Shift+R");
    assert_eq!(chord(MOD_CTRL), "Ctrl+R");
    assert!(chord(MOD_META | 1 << 7).contains('?'), "a modifier with no name was not marked");
    let unnamed = Chord { id: 9, mods: MOD_META, code: 99, action: Action::Rename, does: "" };
    assert_eq!(unnamed.keys(), "Super+?");
}

/// The compositor holds `MAX_HOTKEYS` chords and refuses the rest, the last registered first.
#[test]
fn the_compositor_holds_every_chord() {
    assert!(CHORDS.len() <= librsproto::surface::MAX_HOTKEYS, "{} chords", CHORDS.len());
}

/// A key's name is what that key types, in the keymap the input server reads keys with, so a
/// chord's keycode cannot be another key's under the right name.
#[test]
fn each_key_is_named_as_the_keymap_types_it() {
    for c in CHORDS {
        let typed = libinput::keymap::to_char(c.code, 0).map(|b| char::from(b).to_ascii_uppercase().to_string());
        assert_eq!(key_name(c.code).map(str::to_string), typed, "chord {}", c.id);
    }
}

/// The rows of the table under a page's `## Chords`, each a chord's keys and what it does; every
/// row but the header and its separator, and an error for a key cell that is not one code span.
fn chord_rows(page: &str) -> Result<Vec<(String, String)>, String> {
    let section = page.split("\n## Chords\n").nth(1).ok_or("the page has no `## Chords` section")?;
    let section = section.split("\n## ").next().unwrap_or_default();
    let lines: Vec<&str> = section.lines().filter(|l| l.trim_start().starts_with('|')).collect();
    let is_rule = |l: &str| l.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
    let body = match lines[..] {
        [_, rule, ref body @ ..] if is_rule(rule) => body,
        [] => &[][..],
        _ => return Err("the chord table has no header and rule".into()),
    };
    let mut rows = Vec::new();
    for line in body {
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').map(str::trim).collect();
        let keys = cells[0].strip_prefix('`').and_then(|k| k.strip_suffix('`')).filter(|k| !k.contains('`'));
        let (Some(keys), Some(does)) = (keys, cells.get(1)) else {
            return Err(format!("a chord row's keys are not one code span: {line}"));
        };
        rows.push((keys.to_string(), does.to_string()));
    }
    Ok(rows)
}

#[test]
fn the_page_lists_every_chord_and_no_other() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manual/7/desktop.md");
    let page = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let table: Vec<(String, String)> = CHORDS.iter().map(|c| (c.keys(), c.does.to_string())).collect();
    assert_eq!(
        chord_rows(&page),
        Ok(table),
        "desktop(7)'s chord table, in order, against the chords the shell registers"
    );
}

#[test]
fn a_chord_row_is_read_only_under_its_heading() {
    let page = "# d\n\n| `Super+X` | elsewhere |\n\n## Chords\n\n| Keys | Does |\n|---|---|\n\
                | `Super+H` | Minimize the focused window |\n\n## Menus\n\n| `Super+Y` | after |\n";
    assert_eq!(chord_rows(page), Ok(vec![("Super+H".to_string(), "Minimize the focused window".to_string())]));
}

#[test]
fn a_chord_row_not_written_as_one_code_span_is_refused() {
    let table = |row: &str| alloc::format!("## Chords\n\n| Keys | Does |\n|---|---|\n{row}\n");
    for row in ["| Super+Q | Close |", "| **`Super+Tab`** | Next |", "| `Alt`+`Tab` | Next |", "| `Super+Q` |"] {
        assert!(chord_rows(&table(row)).is_err(), "{row} was read as a chord");
    }
    assert!(chord_rows("## Chords\n\n| `Super+H` | Minimize |\n").is_err(), "a table with no header");
}
