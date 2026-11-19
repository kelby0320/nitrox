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

/// The rows of the table under a page's `## Chords`: each a chord's keys and what it does.
fn chord_rows(page: &str) -> Vec<(String, String)> {
    let section = page.split("\n## Chords\n").nth(1).expect("the page has a `## Chords` section");
    let section = section.split("\n## ").next().unwrap_or_default();
    let mut rows = Vec::new();
    for line in section.lines().filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if let [keys, does, ..] = cells[..]
            && let Some(keys) = keys.strip_prefix('`').and_then(|k| k.strip_suffix('`'))
        {
            rows.push((keys.to_string(), does.to_string()));
        }
    }
    rows
}

#[test]
fn the_page_lists_every_chord_and_no_other() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manual/7/desktop.md");
    let page = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let table: Vec<(String, String)> = CHORDS.iter().map(|c| (c.keys(), c.does.to_string())).collect();
    assert_eq!(chord_rows(&page), table, "desktop(7)'s chord table, in order, against the chords the shell registers");
}

#[test]
fn a_chord_row_is_read_only_under_its_heading() {
    let page = "# d\n\n| `Super+X` | elsewhere |\n\n## Chords\n\n| Keys | Does |\n|---|---|\n\
                | `Super+H` | Minimize the focused window |\n\n## Menus\n\n| `Super+Y` | after |\n";
    assert_eq!(chord_rows(page), [("Super+H".to_string(), "Minimize the focused window".to_string())]);
}
