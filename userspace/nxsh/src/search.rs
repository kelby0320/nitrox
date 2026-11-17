//! Reverse search (`Ctrl-R`, §11c): what each key does to a search, kept pure to be host-tested.

use crate::history::History;
use alloc::string::String;

/// Reverse-search state: the query and the current match. What it shows is the line discipline's
/// line while it lasts, so drawing and erasing it are the discipline's, across a wrap too.
pub struct Search {
    query: String,
    /// The history entry the query matches, or empty.
    pub matched: String,
    at: Option<usize>,
    /// The line as it was when the search began, restored by a cancel.
    pub original: String,
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
        Search { query: String::new(), matched: String::new(), at: None, original }
    }

    /// What the search shows in the line's place.
    ///
    /// The query is quoted and an arrow separates it from the match, so a colon in the query is not
    /// ambiguous. A query with no match says so, rather than looking like a search in progress.
    pub fn display(&self) -> String {
        if self.query.is_empty() {
            String::from("search '' -> ")
        } else if self.matched.is_empty() {
            alloc::format!("search '{}' -> (no match)", self.query)
        } else {
            alloc::format!("search '{}' -> {}", self.query, self.matched)
        }
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
    use alloc::vec::Vec;

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
    fn the_search_shows_its_query_and_its_match() {
        let h = history(&["caf\u{e9}"]);
        let mut st = Search::new(String::new());
        assert_eq!(st.display(), "search '' -> ");
        search_key(&mut st, b'c', &h);
        assert_eq!(st.display(), "search 'c' -> caf\u{e9}");
    }

    #[test]
    fn a_query_with_no_match_says_so() {
        let mut st = Search::new(String::new());
        search_key(&mut st, b'z', &History::new());
        assert_eq!(st.display(), "search 'z' -> (no match)");
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
