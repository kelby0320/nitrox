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
