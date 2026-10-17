//! Command history and reverse-search.
//!
//! TODO(history-pager): `docs/rationale/deferred-decisions.md`.
//!
//! Pure logic: the terminal work (reading keys, redrawing) is the binary's, deciding what to
//! recall is here. History belongs to the shell rather than the tty server because completion,
//! search and recall are one editing loop over one buffer.

use alloc::string::String;
use alloc::vec::Vec;

/// How many lines are kept: a shell in a fixed-size heap cannot grow without bound.
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
    /// Consecutive duplicates are not recorded, so Up does not walk past them.
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

    /// Walk one entry older; `current` is stashed on the first step for [`newer`](Self::newer).
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
    /// Returns the index so repeated `Ctrl-R` can continue from it. An empty query matches nothing:
    /// matching everything would jump the line to an unrelated command on the first keystroke.
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

    #[test]
    fn consecutive_duplicates_are_not_recorded() {
        let mut h = History::new();
        h.push("same");
        h.push("same");
        assert_eq!(h.entries().len(), 1);
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
