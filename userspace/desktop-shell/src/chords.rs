//! The desktop's chords, registered from [`CHORDS`]; `desktop(7)`'s chord table is held to it.

use librsproto::surface::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT, MgrHotkey};

/// `EV_KEY` codes the chords use; named here, as `libkern::abi` lists only the keys the ABI needs.
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_R: u16 = 19;
const KEY_A: u16 = 30;
const KEY_H: u16 = 35;

/// What the shell does when a chord is pressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Make the Nth desktop, as the switcher counts them from 1, the current one.
    Switch(usize),
    /// Move the focused window to the Nth desktop.
    MoveTo(usize),
    /// Ask for a name for the current desktop.
    Rename,
    /// Open the Applications menu, or put away whichever popup is up.
    Apps,
    /// Minimize the focused window.
    Minimize,
}

/// One chord: the keys, the action, and what `desktop(7)` says it does.
#[derive(Clone, Copy, Debug)]
pub struct Chord {
    /// The identity the compositor reports it by; never zero, and never another chord's.
    pub id: u32,
    /// The modifiers held, matched exactly.
    pub mods: u16,
    /// The key pressed with them.
    pub code: u16,
    /// What the shell does.
    pub action: Action,
    /// What it does, as the page's row says.
    pub does: &'static str,
}

/// How many desktops the number chords reach: each costs two of the compositor's sixteen.
pub const CHORD_DESKTOPS: usize = 4;

/// Every chord, in the order the shell registers them.
pub const CHORDS: &[Chord] = &[
    Chord { id: 11, mods: MOD_META, code: KEY_1, action: Action::Switch(1), does: "Go to desktop 1" },
    Chord { id: 12, mods: MOD_META, code: KEY_2, action: Action::Switch(2), does: "Go to desktop 2" },
    Chord { id: 13, mods: MOD_META, code: KEY_3, action: Action::Switch(3), does: "Go to desktop 3" },
    Chord { id: 14, mods: MOD_META, code: KEY_4, action: Action::Switch(4), does: "Go to desktop 4" },
    Chord {
        id: 21,
        mods: MOD_META | MOD_SHIFT,
        code: KEY_1,
        action: Action::MoveTo(1),
        does: "Move the focused window to desktop 1",
    },
    Chord {
        id: 22,
        mods: MOD_META | MOD_SHIFT,
        code: KEY_2,
        action: Action::MoveTo(2),
        does: "Move the focused window to desktop 2",
    },
    Chord {
        id: 23,
        mods: MOD_META | MOD_SHIFT,
        code: KEY_3,
        action: Action::MoveTo(3),
        does: "Move the focused window to desktop 3",
    },
    Chord {
        id: 24,
        mods: MOD_META | MOD_SHIFT,
        code: KEY_4,
        action: Action::MoveTo(4),
        does: "Move the focused window to desktop 4",
    },
    Chord { id: 2, mods: MOD_META, code: KEY_R, action: Action::Rename, does: "Name the current desktop" },
    // `Super+A`, not a tap of `Super`: every chord here begins with `Super` going down.
    Chord {
        id: 3,
        mods: MOD_META,
        code: KEY_A,
        action: Action::Apps,
        does: "Open the Applications menu; again, close whatever menu or prompt is open",
    },
    Chord { id: 1, mods: MOD_META, code: KEY_H, action: Action::Minimize, does: "Minimize the focused window" },
];

impl Chord {
    /// The body the compositor is asked to register.
    pub fn hotkey(&self) -> MgrHotkey {
        MgrHotkey { id: self.id, mods: self.mods, code: self.code }
    }

    /// How a person writes it, `Super+Shift+1`: from what is registered, so the two cannot differ.
    /// A modifier or key it has no name for is a `?`.
    pub fn keys(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        let named = [(MOD_META, "Super+"), (MOD_CTRL, "Ctrl+"), (MOD_ALT, "Alt+"), (MOD_SHIFT, "Shift+")];
        for (bit, name) in named {
            if self.mods & bit != 0 {
                out.push_str(name);
            }
        }
        if self.mods & !named.iter().fold(0, |all, (bit, _)| all | bit) != 0 {
            out.push_str("?+");
        }
        out.push_str(key_name(self.code).unwrap_or("?"));
        out
    }
}

/// The name of a key a chord uses, as its keycap reads.
fn key_name(code: u16) -> Option<&'static str> {
    Some(match code {
        KEY_1 => "1",
        KEY_2 => "2",
        KEY_3 => "3",
        KEY_4 => "4",
        KEY_R => "R",
        KEY_A => "A",
        KEY_H => "H",
        _ => return None,
    })
}

/// What the chord the compositor reported as `id` does, if it is one of [`CHORDS`].
pub fn action(id: u32) -> Option<Action> {
    CHORDS.iter().find(|c| c.id == id).map(|c| c.action)
}

#[cfg(test)]
mod tests;
