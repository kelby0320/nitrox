//! The interactive prompt's editing: each byte typed, through the line discipline, reverse search
//! and completion, to what the loop writes and runs. Pure, so a key's effect is host-tested.

use crate::complete::{Action, Completion, listing};
use crate::history::History;
use crate::search::{Search, SearchStep, search_key};
use alloc::string::String;
use alloc::vec::Vec;
use tty_server::{Discipline, Key, Step};

/// What the loop does after a byte, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// Write these bytes to the terminal.
    Write(Vec<u8>),
    /// Write the prompt, which shows the working directory the loop knows.
    Prompt,
    /// Write the continuation prompt, mid-statement.
    Continuation,
    /// Run this statement, then write the prompt.
    Run(String),
    /// The input has ended.
    Eof,
}

/// The prompt's state: the line being typed, a search in progress, and the statement so far.
pub struct Editor {
    disc: Discipline,
    search: Option<Search>,
    /// The statement's earlier lines, when one continues past a line's end.
    pending: String,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// An empty prompt.
    pub fn new() -> Editor {
        Editor { disc: Discipline::new(), search: None, pending: String::new() }
    }

    /// The line as typed so far.
    pub fn line(&self) -> &[u8] {
        self.disc.line()
    }

    /// `Ctrl-C` at the prompt (§11h): the line, a search and a statement in progress are dropped.
    /// What to write comes back, the rest of the line passed first so `^C` lands after it.
    pub fn interrupt(&mut self) -> Vec<u8> {
        self.search = None;
        self.pending.clear();
        let mut out = self.disc.edit(Key::End);
        out.extend_from_slice(b"^C\r\n");
        self.disc.reset();
        out
    }

    /// One byte typed: what to write and run, in order. `next` is the byte after it in the same
    /// read, if any; `complete` is the interpreter's completion of a statement (§11c).
    pub fn feed(
        &mut self,
        b: u8,
        next: Option<u8>,
        history: &mut History,
        complete: &mut dyn FnMut(&str) -> Completion,
    ) -> Vec<Out> {
        if let Some(st) = self.search.as_mut() {
            return match search_key(st, b, history) {
                SearchStep::Redraw => alloc::vec![Out::Write(st.redraw())],
                SearchStep::Cancel => {
                    let mut out = st.erase();
                    out.extend_from_slice(&self.disc.replace_line(st.original.as_bytes()));
                    self.search = None;
                    alloc::vec![Out::Write(out)]
                }
                SearchStep::Keep => {
                    let mut out = st.erase();
                    out.extend_from_slice(&self.disc.replace_line(st.kept().as_bytes()));
                    self.search = None;
                    // As readline decides: the `ESC` begins a cursor key only if the key's next
                    // byte arrived with it. A person's next key comes in a later read, so a bare
                    // Esc takes nothing with it.
                    if matches!(next, Some(b'[' | b'O')) {
                        self.disc.feed(b);
                    }
                    alloc::vec![Out::Write(out)]
                }
                SearchStep::Accept => {
                    let mut out = st.erase();
                    let chosen = st.matched.clone();
                    out.extend_from_slice(&self.disc.replace_line(chosen.as_bytes()));
                    out.extend_from_slice(b"\r\n");
                    self.search = None;
                    let mut outs = alloc::vec![Out::Write(out)];
                    // Submit by feeding the terminator, as a typed line takes.
                    match self.disc.feed(b'\n') {
                        Step::Line { bytes, .. } => {
                            self.pending.push_str(&String::from_utf8_lossy(&bytes));
                            let line = core::mem::take(&mut self.pending);
                            history.push(&line);
                            outs.push(Out::Run(line));
                        }
                        _ => outs.push(Out::Prompt),
                    }
                    outs
                }
                SearchStep::None => Vec::new(),
            };
        }
        // A byte after a bare `ESC` belongs to it, as everywhere in the discipline, so neither
        // Ctrl-R nor Tab starts anything there.
        let escaping = self.disc.in_escape();
        if b == 0x12 && !escaping {
            let original = String::from_utf8_lossy(self.disc.line()).into_owned();
            let mut out = self.disc.replace_line(b"");
            let mut st = Search::new(original);
            out.extend_from_slice(&st.redraw());
            self.search = Some(st);
            return alloc::vec![Out::Write(out)];
        }
        // Tab completes (§11c) what is before the cursor and keeps what follows, intercepted
        // before the discipline as `Ctrl-R` is.
        if b == b'\t' && !escaping {
            return self.tab(complete);
        }
        match self.disc.feed(b) {
            Step::None => Vec::new(),
            Step::Echo(e) => alloc::vec![Out::Write(e)],
            Step::Eof => alloc::vec![Out::Eof],
            Step::Key(k) => {
                let recalled = match k {
                    Key::Up => history.older(self.disc.line()),
                    Key::Down => history.newer(),
                    _ => return alloc::vec![Out::Write(self.disc.edit(k))],
                };
                match recalled {
                    Some(text) => alloc::vec![Out::Write(self.disc.replace_line(text.as_bytes()))],
                    None => Vec::new(),
                }
            }
            Step::Line { bytes, echo } => {
                let mut outs = alloc::vec![Out::Write(echo)];
                self.pending.push_str(&String::from_utf8_lossy(&bytes));
                if matches!(
                    crate::needs_continuation(&self.pending),
                    crate::Continue::Unclosed | crate::Continue::TrailingPipe
                ) {
                    self.pending.push('\n');
                    outs.push(Out::Continuation);
                    return outs;
                }
                let src = core::mem::take(&mut self.pending);
                if src.trim().is_empty() {
                    outs.push(Out::Prompt);
                } else {
                    history.push(&src);
                    outs.push(Out::Run(src));
                }
                outs
            }
        }
    }

    /// Complete what is before the cursor, keeping what follows: a replacement, or a listing of
    /// the choices with the line redrawn under it.
    fn tab(&mut self, complete: &mut dyn FnMut(&str) -> Completion) -> Vec<Out> {
        let (before, after) = self.disc.line().split_at(self.disc.cursor());
        let (line, after) = (String::from_utf8_lossy(before).into_owned(), after.to_vec());
        // The whole statement, not the physical line: the line is reset per line while `pending`
        // accumulates, so completing the line alone would offer command names inside an argument
        // list.
        let mut whole = self.pending.clone();
        whole.push_str(&line);
        let c = complete(&whole);
        // `Nothing` covers both no match and nothing to add; applying the empty common prefix
        // blindly deleted the word being typed.
        match c.action(&whole) {
            Action::Nothing => Vec::new(),
            // `strip_prefix` rather than an index: doing nothing beats a corrupted line.
            Action::Replace(full) => match full.strip_prefix(self.pending.as_str()) {
                Some(new_line) => alloc::vec![Out::Write(self.disc.replace_around(new_line.as_bytes(), &after))],
                None => Vec::new(),
            },
            // The list goes on its own lines, with no cursor addressing, for the serial console.
            Action::List => alloc::vec![
                Out::Write(listing(&c.candidates)),
                if self.pending.is_empty() { Out::Prompt } else { Out::Continuation },
                Out::Write(self.disc.redraw()),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type `reads` at a prompt over `past` history, completing nothing: the statements run, and
    /// the line left being typed.
    fn typed(past: &[&str], reads: &[&[u8]]) -> (Vec<String>, String) {
        let mut h = History::new();
        for p in past {
            h.push(p);
        }
        let mut e = Editor::new();
        let ran = feed_reads(&mut e, &mut h, reads);
        (ran, String::from_utf8_lossy(e.line()).into_owned())
    }

    fn feed_reads(e: &mut Editor, h: &mut History, reads: &[&[u8]]) -> Vec<String> {
        let mut ran = Vec::new();
        for read in reads {
            for (i, &b) in read.iter().enumerate() {
                for out in e.feed(b, read.get(i + 1).copied(), h, &mut |w| Completion::none(w.len())) {
                    if let Out::Run(src) = out {
                        ran.push(src);
                    }
                }
            }
        }
        ran
    }

    #[test]
    fn a_bare_escape_ending_a_search_leaves_the_next_key_alone() {
        let ran = |next: &[u8]| typed(&["whoami"], &[b"\x12whoa", b"\x1b", next]).0;
        assert_eq!(ran(b"\r"), ["whoami"], "the Enter after Esc runs");
        assert_eq!(ran(b" -x\r"), ["whoami -x"], "the space after Esc is typed");
        assert_eq!(ran(b"O\r"), ["whoamiO"]);
        assert_eq!(ran(b"[x]\r"), ["whoami[x]"], "a `[` typed after Esc is a bracket");
        assert_eq!(typed(&["whoami"], &[b"\x12whoa\x1b", b"\r"]).0, ["whoami"], "Esc last in its read");
    }

    #[test]
    fn a_cursor_key_ends_a_search_and_moves_the_cursor() {
        let ran = |key: &[u8]| typed(&["whoami"], &[b"\x12whoa", key]).0;
        assert_eq!(ran(b"\x1b[Dx\r"), ["whoamxi"]);
        assert_eq!(ran(b"\x1b[H\x1b[3~\r"), ["hoami"]);
        assert_eq!(ran(b"\x1bOH_\r"), ["_whoami"], "a terminal in application mode");
        assert_eq!(typed(&["whoami"], &[b"\x12whoa\x1b[Dx\r"]).0, ["whoamxi"], "the search and the key in one read");
    }

    #[test]
    fn a_search_after_one_ended_by_escape_runs_its_own_match() {
        assert_eq!(typed(&["whoami", "date"], &[b"\x12who", b"\x1b", b"\x12dat", b"\r"]).0, ["date"]);
        assert_eq!(typed(&["whoami", "date"], &[b"\x12who\x1b\x12dat\r"]).0, ["date"]);
    }

    #[test]
    fn an_escape_typed_at_the_prompt_does_not_strand_a_search_or_a_completion() {
        // A bare Esc takes the byte after it, as it always has; Ctrl-R or Tab there starts nothing
        // the line discipline cannot finish.
        assert_eq!(typed(&[], &[b"\x1b", b"\x12", b"abc\r"]).0, ["abc"]);
        assert_eq!(typed(&[], &[b"\x1b", b"\t", b"abc\r"]).0, ["abc"]);
    }

    #[test]
    fn ctrl_c_drops_a_search_and_a_statement_in_progress() {
        let mut h = History::new();
        h.push("whoami");
        let mut e = Editor::new();
        feed_reads(&mut e, &mut h, &[b"if true {\r", b"\x12who"]);
        e.interrupt();
        assert_eq!(feed_reads(&mut e, &mut h, &[b"1\r"]), ["1"]);
    }
}
