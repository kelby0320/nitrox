//! A regex matcher for `~=` (§10b).
//!
//! Milestone 3 Part G. Pattern → program → **Pike VM** (a Thompson NFA simulation):
//! linear time in the input, no backtracking, no catastrophic blowup.
//!
//! ## Why this is small
//!
//! §10b asks `~=` for a boolean — `ls | filter name ~= /\.rs$/` — and `capture` for the
//! text a pattern matched. Everything else a regex library usually carries (replace, named
//! groups, lookaround) is still out.
//!
//! **Submatch extraction is the single largest source of complexity in a regex engine**,
//! and it is confined here to one function: [`Regex::captures`] runs a second, slot-
//! carrying pass. [`Regex::is_match`] is untouched and allocates no slots — `~=` runs once
//! per row inside `filter`, so it must not pay for a feature it never uses.
//!
//! That is also why the minimal version is the *architecturally correct* one rather than a
//! stepping stone: adding counted repetition or character classes later extends the
//! compiler, and the VM does not change.
//!
//! ## What is excluded, and why each is principled
//!
//! - **Backreferences (`\1`).** These are precisely what force backtracking and
//!   exponential blowup. Excluding them is what *permits* the linear-time VM — the same
//!   call Go's `regexp` and RE2 make, for the same reason.
//! - **Counted repetition (`{n,m}`).** Pure sugar; it expands to concatenation. Adding it
//!   later changes no existing pattern's meaning.
//! - **Lazy quantifiers (`*?`).** Only meaningful when extracting submatches, which a
//!   predicate never does.
//! - **Lookaround.** Needs a different engine, and no shell filter has badly needed it.
//!
//! Every one of those is a **loud compile error**, not a silently different match. That is
//! the property that made this the right starting point and literal-substring-matching the
//! wrong one: substring-first would silently change what `.` means the day a real engine
//! arrived, and every pattern written in the meantime would quietly mean something else.
//! Here, a pattern that compiles today means the same thing forever.

use alloc::string::String;
use alloc::vec::Vec;

/// A compiled pattern.
pub struct Regex {
    prog: Vec<Inst>,
    /// Where the program begins.
    ///
    /// **Not instruction zero.** Fragments are emitted as they are parsed and a combinator
    /// emits its own instruction *after* its operands, so an alternation's `Split` lands
    /// past both branches — `a|b` compiles to `Char(a); Char(b); Split(0, 1)`. Starting the
    /// VM at zero therefore ran the *first branch only*, and `"b" ~= /a|b/` was false. It
    /// went unnoticed because a concatenation does start at zero, and no test used a
    /// top-level alternation whose second branch had to win.
    start: usize,
    /// `2 × (groups + 1)`: a start and an end for the whole match and for each group.
    slots: usize,
}

/// One VM instruction.
///
/// Every instruction with a single successor carries it **explicitly**. The tempting
/// shortcut — let a `Char` fall through to `pc + 1` and patch a fragment's exit by
/// overwriting that slot with a `Jump` — is wrong, and wrong in a way that passes simple
/// tests: overwriting the slot destroys the instruction that was there, so `^bc` became
/// `Jump; Char(b); Char(c)` and matched anywhere. Explicit successors make a fragment's
/// exit a *field* to fill rather than an instruction to clobber.
#[derive(Clone, Debug)]
enum Inst {
    /// Consume one character if it satisfies the class, then go to `.1`.
    Char(Class, usize),
    /// Fork: try both, in the same step.
    Split(usize, usize),
    /// Record the current position in slot `.0`, consuming nothing, then go to `.1`.
    ///
    /// Slots are the *only* thing capture adds to the program: what matches is decided by
    /// the same instructions as before, and these record where.
    Save(usize, usize),
    /// Assert start-of-input, consuming nothing, then go to `.0`.
    AssertStart(usize),
    AssertEnd(usize),
    Match,
}

/// What a single position may consume.
#[derive(Clone, Debug)]
enum Class {
    Literal(char),
    Any,
    /// `[abc]`, `[a-z]`, `[^…]` — ranges plus singles, optionally negated.
    Set { ranges: Vec<(char, char)>, negated: bool },
}

impl Class {
    fn matches(&self, c: char) -> bool {
        match self {
            // `.` matches any character except a newline, the near-universal convention.
            Class::Any => c != '\n',
            Class::Literal(l) => *l == c,
            Class::Set { ranges, negated } => {
                let hit = ranges.iter().any(|(lo, hi)| c >= *lo && c <= *hi);
                hit != *negated
            }
        }
    }
}

impl Regex {
    /// Compile a pattern, or say precisely what is not supported.
    pub fn new(pattern: &str) -> Result<Regex, String> {
        let mut p =
            Parser { src: pattern.chars().collect(), pos: 0, prog: Vec::new(), groups: 0 };
        let frag = p.alternation()?;
        if p.pos < p.src.len() {
            return Err(alloc::format!(
                "unexpected `{}` in a regex — an unmatched `)`?",
                p.src[p.pos]
            ));
        }
        let mut prog = p.prog;
        // Patch the fragment's dangling exits to a final `Match`.
        let end = prog.len();
        prog.push(Inst::Match);
        patch(&mut prog, &frag.out, end);
        Ok(Regex { prog, start: frag.start, slots: 2 * (p.groups + 1) })
    }

    /// Whether `text` contains a match (§10b's predicate).
    ///
    /// Unanchored by default, like `grep`: `name ~= /rs/` is true of `parse.rs`. `^` and
    /// `$` anchor explicitly, which is what `/\.rs$/` relies on.
    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        // The Pike VM: one pass over the input carrying a *set* of active threads, so the
        // cost is O(len(text) × len(prog)) regardless of how the pattern nests. No thread
        // ever backs up, which is what removes the blowup.
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        let mut on_current = alloc::vec![false; self.prog.len()];
        let mut on_next = alloc::vec![false; self.prog.len()];

        for start in 0..=chars.len() {
            // Unanchored: a new thread may begin at every position.
            self.add_thread(&mut current, &mut on_current, self.start, start, &chars);
            if current.iter().any(|pc| matches!(self.prog[*pc], Inst::Match)) {
                return true;
            }
            if start == chars.len() {
                break;
            }
            let c = chars[start];
            next.clear();
            for slot in on_next.iter_mut() {
                *slot = false;
            }
            for pc in &current {
                if let Inst::Char(class, then) = &self.prog[*pc] {
                    if class.matches(c) {
                        self.add_thread(&mut next, &mut on_next, *then, start + 1, &chars);
                    }
                }
            }
            core::mem::swap(&mut current, &mut next);
            core::mem::swap(&mut on_current, &mut on_next);
        }
        current.iter().any(|pc| matches!(self.prog[*pc], Inst::Match))
    }

    /// The leftmost match and its groups, as `(start, end)` character offsets.
    ///
    /// `None` when the pattern does not match; an element is `None` for a group that did
    /// not participate (`(a)|(b)` leaves one of them unset by construction). Element 0 is
    /// always the whole match.
    ///
    /// This is the Pike VM again, with two additions that give **leftmost-first**
    /// semantics — the same rule Perl and RE2's default use:
    ///
    /// - Threads are processed in priority order, and a thread that reaches `Match`
    ///   **cuts off every lower-priority thread in the same step**. That is what makes
    ///   `(a|ab)` prefer `a`.
    /// - Once a match exists, no new thread is seeded at a later start, so an earlier
    ///   start always wins over a later one.
    ///
    /// Slots are cloned per thread. That is the cost of submatches and the reason `~=`
    /// keeps its own slot-free pass.
    pub fn captures(&self, text: &str) -> Option<Vec<Option<(usize, usize)>>> {
        let chars: Vec<char> = text.chars().collect();
        let mut current: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
        let mut next: Vec<(usize, Vec<Option<usize>>)> = Vec::new();
        let mut on_current = alloc::vec![false; self.prog.len()];
        let mut on_next = alloc::vec![false; self.prog.len()];
        let mut matched: Option<Vec<Option<usize>>> = None;

        for at in 0..=chars.len() {
            if matched.is_none() {
                // Slot 0 is the match start, recorded when the thread is seeded rather
                // than by an instruction — which is why the compiler emits `Save` only for
                // groups, and why the program's entry point did not have to move.
                let mut slots = alloc::vec![None; self.slots];
                slots[0] = Some(at);
                self.add_capturing(&mut current, &mut on_current, self.start, at, &chars, slots);
            }
            next.clear();
            for slot in on_next.iter_mut() {
                *slot = false;
            }
            let mut i = 0;
            while i < current.len() {
                let (pc, slots) = current[i].clone();
                match &self.prog[pc] {
                    Inst::Match => {
                        let mut done = slots;
                        done[1] = Some(at);
                        matched = Some(done);
                        break; // cut the lower-priority threads
                    }
                    Inst::Char(class, then) => {
                        if at < chars.len() && class.matches(chars[at]) {
                            self.add_capturing(
                                &mut next,
                                &mut on_next,
                                *then,
                                at + 1,
                                &chars,
                                slots,
                            );
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            core::mem::swap(&mut current, &mut next);
            core::mem::swap(&mut on_current, &mut on_next);
        }

        matched.map(|slots| {
            (0..self.slots / 2)
                .map(|g| match (slots[2 * g], slots[2 * g + 1]) {
                    (Some(a), Some(b)) => Some((a, b)),
                    _ => None,
                })
                .collect()
        })
    }

    /// [`Regex::add_thread`]'s twin, carrying slots. Kept separate rather than
    /// generic because the whole point is that `is_match` allocates none.
    fn add_capturing(
        &self,
        list: &mut Vec<(usize, Vec<Option<usize>>)>,
        seen: &mut [bool],
        pc: usize,
        at: usize,
        chars: &[char],
        slots: Vec<Option<usize>>,
    ) {
        if pc >= self.prog.len() || seen[pc] {
            return;
        }
        seen[pc] = true;
        match &self.prog[pc] {
            Inst::Split(a, b) => {
                let (a, b) = (*a, *b);
                // Priority order: `a` first. This is what makes the match leftmost-*first*
                // rather than longest.
                self.add_capturing(list, seen, a, at, chars, slots.clone());
                self.add_capturing(list, seen, b, at, chars, slots);
            }
            Inst::Save(n, then) => {
                let (n, then) = (*n, *then);
                let mut slots = slots;
                slots[n] = Some(at);
                self.add_capturing(list, seen, then, at, chars, slots);
            }
            Inst::AssertStart(then) => {
                let then = *then;
                if at == 0 {
                    self.add_capturing(list, seen, then, at, chars, slots);
                }
            }
            Inst::AssertEnd(then) => {
                let then = *then;
                if at == chars.len() {
                    self.add_capturing(list, seen, then, at, chars, slots);
                }
            }
            _ => list.push((pc, slots)),
        }
    }

    /// Follow `Split`/`Jump`/assertions without consuming, adding reachable states.
    ///
    /// `seen` is what keeps this linear: a state already in the set this step is not added
    /// twice, so a pattern like `(a*)*` cannot spin.
    fn add_thread(&self, list: &mut Vec<usize>, seen: &mut [bool], pc: usize, at: usize, chars: &[char]) {
        if pc >= self.prog.len() || seen[pc] {
            return;
        }
        seen[pc] = true;
        match &self.prog[pc] {
            Inst::Split(a, b) => {
                let (a, b) = (*a, *b);
                self.add_thread(list, seen, a, at, chars);
                self.add_thread(list, seen, b, at, chars);
            }
            // `is_match` does not care *where* anything matched, so a `Save` is a plain
            // pass-through here — the slot-carrying twin in `captures` is where it records.
            Inst::Save(_, then) => {
                let then = *then;
                self.add_thread(list, seen, then, at, chars);
            }
            Inst::AssertStart(then) => {
                let then = *then;
                if at == 0 {
                    self.add_thread(list, seen, then, at, chars);
                }
            }
            Inst::AssertEnd(then) => {
                let then = *then;
                if at == chars.len() {
                    self.add_thread(list, seen, then, at, chars);
                }
            }
            _ => list.push(pc),
        }
    }
}

/// A compiled fragment: where it starts, and the instruction slots that still need an exit.
struct Frag {
    start: usize,
    out: Vec<Hole>,
}

/// A dangling exit: which instruction, and which of its two slots.
#[derive(Copy, Clone)]
enum Hole {
    Next(usize),
    SplitA(usize),
    SplitB(usize),
}

fn patch(prog: &mut [Inst], holes: &[Hole], target: usize) {
    for h in holes {
        match *h {
            // Fill the instruction's successor *field*; never replace the instruction.
            Hole::Next(i) => match &mut prog[i] {
                Inst::Char(_, then)
                | Inst::AssertStart(then)
                | Inst::AssertEnd(then)
                | Inst::Save(_, then) => {
                    *then = target;
                }
                _ => {}
            },
            Hole::SplitA(i) => {
                if let Inst::Split(a, _) = &mut prog[i] {
                    *a = target;
                }
            }
            Hole::SplitB(i) => {
                if let Inst::Split(_, b) = &mut prog[i] {
                    *b = target;
                }
            }
        }
    }
}

struct Parser {
    src: Vec<char>,
    pos: usize,
    prog: Vec<Inst>,
    /// How many capturing groups have been opened, which is also the last group's number.
    groups: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn emit(&mut self, i: Inst) -> usize {
        self.prog.push(i);
        self.prog.len() - 1
    }

    /// `a|b|c`
    fn alternation(&mut self) -> Result<Frag, String> {
        let mut left = self.concat()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            let right = self.concat()?;
            let split = self.emit(Inst::Split(left.start, right.start));
            let mut out = left.out;
            out.extend(right.out);
            left = Frag { start: split, out };
        }
        Ok(left)
    }

    fn concat(&mut self) -> Result<Frag, String> {
        let mut frag: Option<Frag> = None;
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let piece = self.repeat()?;
            frag = Some(match frag {
                None => piece,
                Some(prev) => {
                    patch(&mut self.prog, &prev.out, piece.start);
                    Frag { start: prev.start, out: piece.out }
                }
            });
        }
        Ok(match frag {
            Some(f) => f,
            // An empty branch (`a|`) matches the empty string.
            None => {
                // An empty branch (`a|`) matches the empty string: a `Split` whose two
                // arms are the same exit, so it consumes nothing and always proceeds.
                let sp = self.emit(Inst::Split(usize::MAX, usize::MAX));
                Frag { start: sp, out: alloc::vec![Hole::SplitA(sp), Hole::SplitB(sp)] }
            }
        })
    }

    /// A single item with its optional `*`, `+` or `?`.
    fn repeat(&mut self) -> Result<Frag, String> {
        let atom = self.atom()?;
        let Some(c) = self.peek() else { return Ok(atom) };
        let frag = match c {
            '*' => {
                self.pos += 1;
                let split = self.emit(Inst::Split(atom.start, usize::MAX));
                patch(&mut self.prog, &atom.out, split);
                Frag { start: split, out: alloc::vec![Hole::SplitB(split)] }
            }
            '+' => {
                self.pos += 1;
                let split = self.emit(Inst::Split(atom.start, usize::MAX));
                patch(&mut self.prog, &atom.out, split);
                Frag { start: atom.start, out: alloc::vec![Hole::SplitB(split)] }
            }
            '?' => {
                self.pos += 1;
                let split = self.emit(Inst::Split(atom.start, usize::MAX));
                let mut out = atom.out;
                out.push(Hole::SplitB(split));
                Frag { start: split, out }
            }
            _ => return Ok(atom),
        };
        // A second quantifier is a lazy/possessive marker, which a predicate has no use
        // for — and refusing is what keeps every accepted pattern's meaning stable.
        if matches!(self.peek(), Some('?') | Some('*') | Some('+')) {
            return Err(String::from(
                "lazy and possessive quantifiers (`*?`, `+?`) are not supported — they only \
                 matter when extracting submatches, and `~=` is a predicate",
            ));
        }
        Ok(frag)
    }

    fn atom(&mut self) -> Result<Frag, String> {
        let Some(c) = self.peek() else {
            return Err(String::from("a regex ended where a pattern was expected"));
        };
        match c {
            '(' => {
                self.pos += 1;
                // `(?=…)`, `(?:…)`, `(?…` — all refused by name rather than mis-parsed.
                if self.peek() == Some('?') {
                    return Err(String::from(
                        "`(?…)` groups (lookaround, non-capturing) are not supported — \
                         lookaround needs a different engine",
                    ));
                }
                // Groups are numbered in the order their `(` appears, which is the order
                // a reader counts them in.
                self.groups += 1;
                let g = self.groups;
                let open = self.emit(Inst::Save(2 * g, usize::MAX));
                let inner = self.alternation()?;
                if self.peek() != Some(')') {
                    return Err(String::from("unclosed `(` in a regex"));
                }
                self.pos += 1;
                let close = self.emit(Inst::Save(2 * g + 1, usize::MAX));
                patch(&mut self.prog, &[Hole::Next(open)], inner.start);
                patch(&mut self.prog, &inner.out, close);
                Ok(Frag { start: open, out: alloc::vec![Hole::Next(close)] })
            }
            '[' => {
                self.pos += 1;
                let class = self.char_class()?;
                let i = self.emit(Inst::Char(class, usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
            '.' => {
                self.pos += 1;
                let i = self.emit(Inst::Char(Class::Any, usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
            '^' => {
                self.pos += 1;
                let i = self.emit(Inst::AssertStart(usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
            '$' => {
                self.pos += 1;
                let i = self.emit(Inst::AssertEnd(usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
            '{' => Err(String::from(
                "counted repetition `{n,m}` is not supported — it is sugar for \
                 concatenation and can be added without changing any existing pattern",
            )),
            '*' | '+' | '?' => Err(alloc::format!("`{c}` has nothing to repeat")),
            ')' => Err(String::from("unmatched `)` in a regex")),
            '\\' => {
                self.pos += 1;
                let Some(e) = self.peek() else {
                    return Err(String::from("a regex ended in a backslash"));
                };
                self.pos += 1;
                let class = escape_class(e)?;
                let i = self.emit(Inst::Char(class, usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
            _ => {
                self.pos += 1;
                let i = self.emit(Inst::Char(Class::Literal(c), usize::MAX));
                Ok(Frag { start: i, out: alloc::vec![Hole::Next(i)] })
            }
        }
    }

    /// `[abc]`, `[a-z]`, `[^…]`.
    fn char_class(&mut self) -> Result<Class, String> {
        let negated = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut ranges: Vec<(char, char)> = Vec::new();
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return Err(String::from("unclosed `[` in a regex"));
            };
            // A `]` immediately after `[` or `[^` is a literal, the POSIX convention.
            if c == ']' && !first {
                self.pos += 1;
                if ranges.is_empty() {
                    return Err(String::from("an empty character class matches nothing"));
                }
                return Ok(Class::Set { ranges, negated });
            }
            first = false;
            self.pos += 1;
            let lo = if c == '\\' {
                let Some(e) = self.peek() else {
                    return Err(String::from("a character class ended in a backslash"));
                };
                self.pos += 1;
                match escape_class(e)? {
                    Class::Literal(l) => l,
                    // `\d` inside a class contributes its ranges.
                    Class::Set { ranges: r, .. } => {
                        ranges.extend(r);
                        continue;
                    }
                    Class::Any => {
                        return Err(String::from("`\\.` inside a class is a literal dot"));
                    }
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.src.get(self.pos + 1).copied() != Some(']') {
                self.pos += 1;
                let Some(hi) = self.peek() else {
                    return Err(String::from("unclosed `[` in a regex"));
                };
                self.pos += 1;
                if hi < lo {
                    return Err(alloc::format!("range `{lo}-{hi}` runs backwards"));
                }
                ranges.push((lo, hi));
            } else {
                ranges.push((lo, lo));
            }
        }
    }
}

/// `\d`, `\w`, `\s`, and escaped metacharacters.
fn escape_class(e: char) -> Result<Class, String> {
    Ok(match e {
        'd' => Class::Set { ranges: alloc::vec![('0', '9')], negated: false },
        'D' => Class::Set { ranges: alloc::vec![('0', '9')], negated: true },
        'w' => Class::Set {
            ranges: alloc::vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: false,
        },
        'W' => Class::Set {
            ranges: alloc::vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            negated: true,
        },
        's' => Class::Set {
            ranges: alloc::vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            negated: false,
        },
        'S' => Class::Set {
            ranges: alloc::vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            negated: true,
        },
        'n' => Class::Literal('\n'),
        't' => Class::Literal('\t'),
        'r' => Class::Literal('\r'),
        // A digit after a backslash is a backreference — the one exclusion that is
        // load-bearing rather than merely scoped, since backreferences are what would
        // force backtracking and lose the linear-time guarantee.
        '1'..='9' => {
            return Err(String::from(
                "backreferences (`\\1`) are not supported — they are what would force \
                 backtracking, and excluding them is what keeps matching linear-time",
            ));
        }
        // Everything else escapes to itself: `\.`, `\*`, `\/`, `\\`.
        other => Class::Literal(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        Regex::new(pattern)
            .unwrap_or_else(|e| panic!("compiling `{pattern}`: {e}"))
            .is_match(text)
    }

    #[test]
    fn literals_match_unanchored() {
        assert!(m("rs", "parse.rs"));
        // Not `parse.py` — that contains "rs" in "pa*rs*e", which is exactly the sort of
        // accidental substring an unanchored predicate is supposed to find.
        assert!(!m("rs", "notes.py"));
        assert!(m("", "anything"));
    }

    /// The design's own example (§10b), and the reason anchors and escapes both exist.
    #[test]
    fn the_section_10b_example_works() {
        let re = Regex::new(r"\.rs$").expect("compiles");
        assert!(re.is_match("parse.rs"));
        assert!(!re.is_match("parse.rss"));
        assert!(!re.is_match("rs"));
        // `\.` is a literal dot, so it does not match an arbitrary character.
        assert!(!re.is_match("parsexrs"));
    }

    #[test]
    fn anchors_pin_both_ends() {
        assert!(m("^ab", "abc"));
        assert!(!m("^bc", "abc"));
        assert!(m("bc$", "abc"));
        assert!(!m("ab$", "abc"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "abcd"));
    }

    #[test]
    fn quantifiers_and_alternation() {
        assert!(m("ab*c", "ac"));
        assert!(m("ab*c", "abbbc"));
        assert!(m("ab+c", "abc"));
        assert!(!m("ab+c", "ac"));
        assert!(m("ab?c", "ac"));
        assert!(m("^(cat|dog)$", "dog"));
        assert!(!m("^(cat|dog)$", "cow"));
        assert!(m("^(ab)+$", "ababab"));
    }

    #[test]
    fn dot_matches_anything_but_a_newline() {
        assert!(m("a.c", "abc"));
        assert!(m("a.c", "a c"));
        assert!(!m("a.c", "a\nc"));
    }

    #[test]
    fn character_classes_including_ranges_and_negation() {
        assert!(m("^[abc]$", "b"));
        assert!(!m("^[abc]$", "d"));
        assert!(m("^[a-z]+$", "hello"));
        assert!(!m("^[a-z]+$", "Hello"));
        assert!(m("^[^0-9]+$", "abc"));
        assert!(!m("^[^0-9]+$", "ab3"));
        // A `-` at the end of a class is a literal.
        assert!(m("^[a-]$", "-"));
    }

    #[test]
    fn escape_classes() {
        assert!(m(r"^\d+$", "1234"));
        assert!(!m(r"^\d+$", "12a4"));
        assert!(m(r"^\w+$", "a_1"));
        assert!(m(r"^a\sb$", "a b"));
        assert!(m(r"^\D$", "x"));
    }

    /// The property that makes this the right starting point: an unsupported construct is
    /// a **loud error**, never a silently different match. A pattern that compiles today
    /// means the same thing forever.
    #[test]
    fn unsupported_constructs_are_named_errors() {
        let cases = [
            (r"(a)\1", "backreference"),
            ("a{2,3}", "counted repetition"),
            ("a*?", "lazy"),
            ("(?=a)", "lookaround"),
        ];
        for (pattern, expect) in cases {
            let e = match Regex::new(pattern) {
                Ok(_) => panic!("`{pattern}` should not compile"),
                Err(e) => e,
            };
            assert!(
                e.to_lowercase().contains(expect),
                "compiling `{pattern}` should mention {expect}, said: {e}"
            );
        }
    }

    #[test]
    fn malformed_patterns_are_errors_not_silent_acceptance() {
        for bad in ["(ab", "ab)", "[ab", "[]", "*a", "[z-a]", "a\\"] {
            if Regex::new(bad).is_ok() {
                panic!("`{bad}` should not compile");
            }
        }
    }

    /// The whole reason for a Pike VM. A backtracking engine takes exponential time on
    /// this; here it is linear, and the test finishing *is* the assertion.
    #[test]
    fn a_pathological_pattern_does_not_blow_up() {
        let re = Regex::new("^(a*)*b$").expect("compiles");
        assert!(!re.is_match("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(re.is_match("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
        // Nested alternation over a long input, the other classic blowup shape.
        let re = Regex::new("^(a|aa)+$").expect("compiles");
        assert!(re.is_match("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!re.is_match("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
    }

    #[test]
    fn matching_is_over_characters_not_bytes() {
        assert!(m("^.$", "é"));
        assert!(m("^é$", "é"));
    }

    // --- submatches (§10b, Part F) ------------------------------------------

    fn caps(pattern: &str, text: &str) -> Option<Vec<Option<alloc::string::String>>> {
        let re = Regex::new(pattern).expect("compiles");
        let chars: Vec<char> = text.chars().collect();
        re.captures(text).map(|groups| {
            groups
                .iter()
                .map(|g| g.map(|(a, b)| chars[a..b].iter().collect()))
                .collect()
        })
    }

    fn text(pattern: &str, input: &str) -> Vec<alloc::string::String> {
        caps(pattern, input)
            .expect("a match")
            .into_iter()
            .map(|g| g.unwrap_or_else(|| alloc::string::String::from("<none>")))
            .collect()
    }

    #[test]
    fn element_zero_is_the_whole_match() {
        assert_eq!(text("b.d", "abcde"), ["bcd"]);
        assert!(caps("zzz", "abc").is_none());
    }

    #[test]
    fn groups_come_back_in_the_order_they_open() {
        assert_eq!(text(r"(\d+)-(\d+)", "call 12-345 now"), ["12-345", "12", "345"]);
        // Nested groups are numbered by their `(`, which is how a reader counts them.
        assert_eq!(text("((a)b)", "xaby"), ["ab", "ab", "a"]);
    }

    /// A group that did not participate is `None`, not an empty string — `(a)|(b)` leaves
    /// one unset **by construction**, and "did not match" is not "matched nothing".
    #[test]
    fn a_group_that_did_not_participate_is_absent() {
        let g = caps("(a)|(b)", "b").expect("a match");
        assert_eq!(g[0].as_deref(), Some("b"));
        assert_eq!(g[1], None);
        assert_eq!(g[2].as_deref(), Some("b"));
    }

    /// **Leftmost-first**, the rule Perl and RE2's default use: an earlier start wins, and
    /// within a start the higher-priority alternative wins.
    #[test]
    fn the_match_is_leftmost_then_first() {
        assert_eq!(text("(a|ab)", "ab"), ["a", "a"]);
        assert_eq!(text("a+", "baaa"), ["aaa"]);
        // An earlier start beats a longer later one.
        assert_eq!(text("(x|xy)", "zxy"), ["x", "x"]);
    }

    /// Anchors still mean what they meant; slots record *where*, they do not decide *what*.
    #[test]
    fn anchors_are_unaffected_by_slots() {
        assert!(caps("^abc", "xabc").is_none());
        assert_eq!(text("^(a)bc$", "abc"), ["abc", "a"]);
    }

    /// A repeated group keeps its **last** iteration, which is what every engine does and
    /// the reason `(a)*` is a poor way to collect things.
    #[test]
    fn a_repeated_group_holds_its_last_iteration() {
        assert_eq!(text("(ab)+", "ababab"), ["ababab", "ab"]);
    }

    /// The pathological case the engine exists to survive still terminates — now with
    /// slots being cloned at every fork.
    #[test]
    fn the_pathological_pattern_still_terminates_with_slots() {
        let g = caps("(a*)*b", "aaaaaaaaaaaaaaaaaaaaaaaaac");
        assert!(g.is_none());
    }

    /// **A top-level alternation must try every branch**, which it did not.
    ///
    /// Instructions are emitted as fragments are parsed, and a combinator emits its own
    /// after its operands — so `a|b` is `Char(a); Char(b); Split(0, 1)` and the `Split` is
    /// the *last* instruction. Both VMs started at instruction zero, which is the first
    /// branch, so `"b" ~= /a|b/` was false. A concatenation happens to start at zero, and
    /// no test used an alternation whose second branch had to win — found by writing
    /// `capture`'s tests, not by writing `capture`.
    #[test]
    fn an_alternation_tries_both_branches() {
        let re = Regex::new("a|b").expect("compiles");
        assert!(re.is_match("a"));
        assert!(re.is_match("b"), "the second branch was unreachable");
        assert!(!re.is_match("c"));
        // …and the same for `capture`, which shares the entry point.
        assert_eq!(text("(a)|(b)", "b")[0], "b");
        // Nested inside a larger pattern, where the Split is not last either.
        let re = Regex::new("x(a|b)y").expect("compiles");
        assert!(re.is_match("xby"));
    }
}
