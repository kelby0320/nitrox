//! Reverse search (`Ctrl-R`, §11c): what each key does to a search, kept pure to be host-tested.

use crate::history::History;
use alloc::string::String;
use alloc::vec::Vec;

/// Reverse-search state: the query, the current match, and what is on screen.
///
/// The shell tracks its own display length: `replace_line` erases as many characters as the line
/// buffer holds, and a search displays more, which leaves debris on a dumb terminal.
pub struct Search {
    query: String,
    /// The history entry the query matches, or empty.
    pub matched: String,
    at: Option<usize>,
    /// The line as it was when the search began, restored by a cancel.
    pub original: String,
    /// How many characters are currently displayed, so they can be erased exactly.
    shown: usize,
}

/// What a key did to a search.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchStep {
    /// Nothing to show.
    None,
    /// The query or the match changed: redraw.
    Redraw,
    /// Leave the search, restoring the original line (`Ctrl-G`).
    Cancel,
    /// Leave with the match on the line to edit (`ESC`), the `ESC` passed on to start a cursor key.
    Keep,
    /// Take the match and submit it.
    Accept,
}

impl Search {
    /// A search begun from `original`, the line as typed so far.
    pub fn new(original: String) -> Search {
        Search { query: String::new(), matched: String::new(), at: None, original, shown: 0 }
    }

    /// The bytes that erase what the search shows.
    pub fn erase(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.shown * 3);
        for _ in 0..self.shown {
            out.extend_from_slice(b"\x08 \x08");
        }
        self.shown = 0;
        out
    }

    /// Erase and redraw the search line, returning the bytes to write.
    ///
    /// The query is quoted and an arrow separates it from the match, so a colon in the query is not
    /// ambiguous. A query with no match says so, rather than looking like a search in progress.
    pub fn redraw(&mut self) -> Vec<u8> {
        let mut out = self.erase();
        let text = if self.query.is_empty() {
            String::from("search '' -> ")
        } else if self.matched.is_empty() {
            alloc::format!("search '{}' -> (no match)", self.query)
        } else {
            alloc::format!("search '{}' -> {}", self.query, self.matched)
        };
        out.extend_from_slice(text.as_bytes());
        self.shown = text.chars().count();
        out
    }

    /// The line a [`Keep`](SearchStep::Keep) leaves: the match, or the original line if there is
    /// none.
    pub fn kept(&self) -> &str {
        if self.matched.is_empty() { &self.original } else { &self.matched }
    }
}

/// What byte `b` does to the search `st` over `history`.
pub fn search_key(st: &mut Search, b: u8, history: &History) -> SearchStep {
    match b {
        // Ctrl-R again: the next older match.
        0x12 => {
            if let Some(i) = history.search_back(&st.query, st.at) {
                st.at = Some(i);
                st.matched = String::from(history.get(i).unwrap_or(""));
            }
            SearchStep::Redraw
        }
        b'\r' | b'\n' => SearchStep::Accept,
        0x07 => SearchStep::Cancel,
        // As readline's default, so a cursor key edits the match rather than typing `[D` into it.
        0x1b => SearchStep::Keep,
        0x08 | 0x7f => {
            st.query.pop();
            // Re-search from the newest: a shorter query can match something more recent.
            st.at = history.search_back(&st.query, None);
            st.matched = st.at.and_then(|i| history.get(i)).map(String::from).unwrap_or_default();
            SearchStep::Redraw
        }
        0x20..=0x7e => {
            st.query.push(b as char);
            st.at = history.search_back(&st.query, None);
            st.matched = st.at.and_then(|i| history.get(i)).map(String::from).unwrap_or_default();
            SearchStep::Redraw
        }
        _ => SearchStep::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(lines: &[&str]) -> History {
        let mut h = History::new();
        for l in lines {
            h.push(l);
        }
        h
    }

    fn keys(st: &mut Search, h: &History, bytes: &[u8]) -> Vec<SearchStep> {
        bytes.iter().map(|&b| search_key(st, b, h)).collect()
    }

    #[test]
    fn typing_narrows_to_the_newest_match_and_ctrl_r_steps_older() {
        let h = history(&["whoami", "date", "who"]);
        let mut st = Search::new(String::new());
        keys(&mut st, &h, b"who");
        assert_eq!(st.matched, "who");
        search_key(&mut st, 0x12, &h);
        assert_eq!(st.matched, "whoami");
    }

    #[test]
    fn escape_keeps_the_match_and_ctrl_g_restores_the_line() {
        let h = history(&["whoami"]);
        let mut st = Search::new(String::from("typed"));
        keys(&mut st, &h, b"whoa");
        assert_eq!(search_key(&mut st, 0x1b, &h), SearchStep::Keep);
        assert_eq!(st.kept(), "whoami");
        assert_eq!(search_key(&mut st, 0x07, &h), SearchStep::Cancel);
        assert_eq!(st.original, "typed");
    }

    #[test]
    fn escape_with_no_match_keeps_the_original_line() {
        let mut st = Search::new(String::from("typed"));
        keys(&mut st, &History::new(), b"zz");
        assert_eq!(st.kept(), "typed");
    }

    #[test]
    fn enter_accepts_and_backspace_widens() {
        let h = history(&["date", "dated"]);
        let mut st = Search::new(String::new());
        keys(&mut st, &h, b"datex");
        assert!(st.matched.is_empty());
        search_key(&mut st, 0x7f, &h);
        assert_eq!(st.matched, "dated");
        assert_eq!(search_key(&mut st, b'\r', &h), SearchStep::Accept);
    }

    #[test]
    fn redraw_erases_exactly_what_it_showed_last() {
        let h = history(&["caf\u{e9}"]);
        let mut st = Search::new(String::new());
        assert_eq!(st.redraw(), b"search '' -> ");
        search_key(&mut st, b'c', &h);
        let shown = "search 'c' -> caf\u{e9}";
        // The query's own redraw erases the 13 characters of the empty one first.
        assert_eq!(st.redraw(), [b"\x08 \x08".repeat(13), shown.as_bytes().to_vec()].concat());
        assert_eq!(st.erase(), b"\x08 \x08".repeat(shown.chars().count()), "characters, not bytes");
        assert_eq!(st.erase(), b"", "nothing is left to erase");
    }

    #[test]
    fn a_query_with_no_match_says_so() {
        let mut st = Search::new(String::new());
        search_key(&mut st, b'z', &History::new());
        assert_eq!(st.redraw(), b"search 'z' -> (no match)");
    }

    #[test]
    fn a_line_feed_accepts_and_a_space_belongs_to_the_query() {
        let h = history(&["git status", "gitk"]);
        let mut st = Search::new(String::new());
        keys(&mut st, &h, b"git s");
        assert_eq!(st.matched, "git status");
        assert_eq!(search_key(&mut st, b'\n', &h), SearchStep::Accept);
    }
}
