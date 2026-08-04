//! Command history and reverse-search.
//!
//! Pure logic, so it lives in the library half and host-tests in milliseconds — the same
//! rule as the rest of `nxsh`. The terminal work (reading keys, redrawing) is the binary
//! half's; deciding *what* to recall is here.
//!
//! History belongs to the shell rather than the tty server because completion will need the
//! shell's knowledge of commands and schemas, and completion, search and recall are one
//! editing loop over one buffer — see `docs/architecture/console-and-tty.md`.

use alloc::string::String;
use alloc::vec::Vec;

/// How many lines are kept. Bounded because this is a shell in a fixed-size heap, and an
/// unbounded history is a slow leak that only shows up after a long session.
pub const HISTORY_MAX: usize = 128;

/// Command history: a ring, newest last, with a cursor for recall.
pub struct History {
    lines: Vec<String>,
    /// How far back recall has walked. `0` means "at the prompt, not in history".
    back: usize,
    /// What was being typed when recall started, so Down can return to it.
    stash: String,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> History {
        History { lines: Vec::new(), back: 0, stash: String::new() }
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.lines
    }

    /// Record a submitted line.
    ///
    /// Consecutive duplicates are not recorded: pressing Enter twice on the same command
    /// should not cost two presses of Up to walk past.
    pub fn push(&mut self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        if self.lines.last().map(|l| l.as_str()) != Some(line) {
            if self.lines.len() == HISTORY_MAX {
                self.lines.remove(0);
            }
            self.lines.push(String::from(line));
        }
        self.back = 0;
    }

    /// Walk one entry older. `current` is what is on the line now, stashed on the first
    /// step so [`newer`](Self::newer) can restore it.
    pub fn older(&mut self, current: &[u8]) -> Option<&str> {
        if self.back == self.lines.len() {
            return None; // already at the oldest
        }
        if self.back == 0 {
            self.stash = String::from_utf8_lossy(current).into_owned();
        }
        self.back += 1;
        self.lines.get(self.lines.len() - self.back).map(|s| s.as_str())
    }

    /// Walk one entry newer, ending at whatever was being typed before recall started.
    pub fn newer(&mut self) -> Option<&str> {
        if self.back == 0 {
            return None; // already at the prompt
        }
        self.back -= 1;
        if self.back == 0 {
            Some(&self.stash)
        } else {
            self.lines.get(self.lines.len() - self.back).map(|s| s.as_str())
        }
    }

    /// The most recent entry containing `query`, searching strictly older than `before`
    /// (an index into [`entries`](Self::entries)); `None` starts from the newest.
    ///
    /// Returns the index so a caller can continue from it — repeated `Ctrl-R` walks back
    /// through *every* match rather than sticking on the newest one.
    ///
    /// An empty query matches nothing. Matching everything would make the first keystroke
    /// of a search jump the line to an unrelated command.
    pub fn search_back(&self, query: &str, before: Option<usize>) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let start = match before {
            Some(0) => return None, // nothing older than the oldest
            Some(i) => i,
            None => self.lines.len(),
        };
        self.lines[..start].iter().rposition(|l| l.contains(query))
    }

    /// The entry at `i`, if any.
    pub fn get(&self, i: usize) -> Option<&str> {
        self.lines.get(i).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> History {
        let mut h = History::new();
        for l in ["list /", "whoami", "list /bin", "date"] {
            h.push(l);
        }
        h
    }

    #[test]
    fn up_walks_back_and_down_returns() {
        let mut h = seeded();
        assert_eq!(h.older(b""), Some("date"));
        assert_eq!(h.older(b""), Some("list /bin"));
        assert_eq!(h.newer(), Some("date"));
        assert_eq!(h.newer(), Some(""), "back at the prompt");
        assert_eq!(h.newer(), None, "and no further");
    }

    /// The in-progress line is not lost by starting a recall.
    #[test]
    fn what_was_being_typed_comes_back() {
        let mut h = seeded();
        assert_eq!(h.older(b"half-typed"), Some("date"));
        assert_eq!(h.newer(), Some("half-typed"));
    }

    #[test]
    fn walking_past_the_oldest_stops() {
        let mut h = History::new();
        h.push("only");
        assert_eq!(h.older(b""), Some("only"));
        assert_eq!(h.older(b""), None);
    }

    /// Pressing Enter twice on one command should not cost two presses of Up.
    #[test]
    fn consecutive_duplicates_are_not_recorded() {
        let mut h = History::new();
        h.push("same");
        h.push("same");
        assert_eq!(h.entries().len(), 1);
        // ...but the same command later, after something else, is a real entry.
        h.push("other");
        h.push("same");
        assert_eq!(h.entries().len(), 3);
    }

    #[test]
    fn blank_lines_are_not_recorded() {
        let mut h = History::new();
        h.push("   ");
        h.push("");
        assert!(h.entries().is_empty());
    }

    #[test]
    fn the_ring_is_bounded() {
        let mut h = History::new();
        for i in 0..HISTORY_MAX + 10 {
            h.push(&alloc::format!("cmd{i}"));
        }
        assert_eq!(h.entries().len(), HISTORY_MAX);
        assert_eq!(h.entries()[0], "cmd10", "the oldest were dropped, not the newest");
    }

    #[test]
    fn search_finds_the_most_recent_match() {
        let h = seeded();
        let i = h.search_back("list", None).expect("a match");
        assert_eq!(h.get(i), Some("list /bin"));
    }

    /// Repeated `Ctrl-R` must walk through *every* match, not stick on the newest.
    #[test]
    fn search_continues_past_a_match() {
        let h = seeded();
        let first = h.search_back("list", None).expect("first");
        let second = h.search_back("list", Some(first)).expect("second");
        assert_eq!(h.get(second), Some("list /"));
        assert_eq!(h.search_back("list", Some(second)), None, "and then no more");
    }

    #[test]
    fn search_matches_anywhere_in_the_line() {
        let h = seeded();
        let i = h.search_back("bin", None).expect("a match");
        assert_eq!(h.get(i), Some("list /bin"));
    }

    /// An empty query matching everything would make the first keystroke of a search jump
    /// the line to an unrelated command.
    #[test]
    fn an_empty_query_matches_nothing() {
        let h = seeded();
        assert_eq!(h.search_back("", None), None);
    }

    #[test]
    fn a_search_with_no_match_finds_nothing() {
        let h = seeded();
        assert_eq!(h.search_back("zzz", None), None);
    }
}
