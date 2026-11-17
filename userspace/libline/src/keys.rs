//! Typed bytes to keys: an escape sequence becomes the key it names, and any other byte passes.

/// A key a sequence names, reported rather than acted on: canonical mode drops every one, as
/// Linux's does, and `nxsh` recalls history with Up and Down and edits through
/// [`Discipline::edit`](crate::Discipline::edit).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    /// Cursor up (`ESC [ A`).
    Up,
    /// Cursor down (`ESC [ B`).
    Down,
    /// Cursor left (`ESC [ D`).
    Left,
    /// Cursor right (`ESC [ C`).
    Right,
    /// To the line's start (`ESC [ H`, or `ESC [ 1 ~` and `ESC [ 7 ~` as other terminals send it).
    Home,
    /// To the line's end (`ESC [ F`, `ESC [ 4 ~`, `ESC [ 8 ~`).
    End,
    /// Delete the character under the cursor (`ESC [ 3 ~`).
    Delete,
    /// A page back (`ESC [ 5 ~`): the line takes no page, so it is for whoever reads keys.
    PageUp,
    /// A page on (`ESC [ 6 ~`).
    PageDown,
}

/// What one byte typed comes to.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Typed {
    /// Nothing yet: part of a sequence, or a sequence that names no key.
    Nothing,
    /// A byte that is not part of a sequence, for its reader to act on.
    Byte(u8),
    /// The key a sequence named.
    Key(Key),
}

/// Where the escape-sequence parser is; a sequence naming no key is discarded, not accumulated.
#[derive(Copy, Clone, PartialEq, Eq, Default)]
enum EscState {
    #[default]
    Ground,
    Esc,
    /// In a CSI (`ESC [`): its first numeric parameter so far, and whether a `;` has ended it.
    Csi {
        param: u32,
        done: bool,
    },
    /// After `ESC O` (SS3): one final byte follows.
    Ss3,
}

/// The key a CSI's final byte and first parameter name, if it is one this discipline reports.
fn csi_key(fin: u8, param: u32) -> Option<Key> {
    match (fin, param) {
        (b'A', _) => Some(Key::Up),
        (b'B', _) => Some(Key::Down),
        (b'C', _) => Some(Key::Right),
        (b'D', _) => Some(Key::Left),
        (b'H', _) | (b'~', 1 | 7) => Some(Key::Home),
        (b'F', _) | (b'~', 4 | 8) => Some(Key::End),
        (b'~', 3) => Some(Key::Delete),
        (b'~', 5) => Some(Key::PageUp),
        (b'~', 6) => Some(Key::PageDown),
        _ => None,
    }
}

/// Where a sequence is: `Keys::feed` each byte typed, in order.
#[derive(Clone, Default)]
pub struct Keys {
    esc: EscState,
}

impl Keys {
    /// No sequence begun.
    pub const fn new() -> Keys {
        Keys { esc: EscState::Ground }
    }

    /// Whether a sequence is half-read, so the next byte belongs to it.
    pub fn in_escape(&self) -> bool {
        self.esc != EscState::Ground
    }

    /// Abandon a sequence half-read.
    pub fn reset(&mut self) {
        self.esc = EscState::Ground;
    }

    /// One byte typed.
    pub fn feed(&mut self, b: u8) -> Typed {
        // Mid-escape bytes are never text: a `[` after ESC is not a bracket the user typed.
        match self.esc {
            EscState::Ground => {}
            // `ESC` is the meta prefix, so the byte after a bare one is consumed, as readline
            // does: `ESC d` is M-d, and an unbound pair is discarded rather than inserted.
            EscState::Esc => {
                self.esc = match b {
                    b'[' => EscState::Csi { param: 0, done: false },
                    b'O' => EscState::Ss3,
                    _ => EscState::Ground,
                };
                return Typed::Nothing;
            }
            // `ESC O` ends at its final byte, which is never typed. A byte that cannot end it,
            // Enter say, ends the sequence and is itself acted on.
            EscState::Ss3 => {
                self.esc = EscState::Ground;
                match b {
                    0x40..=0x7E => {
                        return csi_key(if matches!(b, b'A'..=b'D' | b'H' | b'F') { b } else { 0 }, 0)
                            .map_or(Typed::Nothing, Typed::Key);
                    }
                    0x1B => {
                        self.esc = EscState::Esc;
                        return Typed::Nothing;
                    }
                    _ => return Typed::Byte(b),
                }
            }
            EscState::Csi { param, done } => {
                // A CSI ends at its *final* byte, not its first: ending `ESC [ 3 ~` (Delete) at
                // the `3` would let the `~` land on the line as printable text and be echoed.
                return match b {
                    // The first parameter names the key (`ESC [ 3 ~`); a `;` ends it.
                    b'0'..=b'9' if !done => {
                        let param = param.saturating_mul(10).saturating_add(u32::from(b - b'0'));
                        self.esc = EscState::Csi { param, done };
                        Typed::Nothing
                    }
                    b';' => {
                        self.esc = EscState::Csi { param, done: true };
                        Typed::Nothing
                    }
                    // Other parameter and intermediate bytes belong to the sequence.
                    0x20..=0x3F => Typed::Nothing,
                    // `ESC` restarts, so an interrupted sequence cannot swallow the next one.
                    0x1B => {
                        self.esc = EscState::Esc;
                        Typed::Nothing
                    }
                    // A final byte ends it; one that names no key leaks nothing into the line.
                    0x40..=0x7E => {
                        self.esc = EscState::Ground;
                        csi_key(b, param).map_or(Typed::Nothing, Typed::Key)
                    }
                    // Malformed: end it rather than swallow the rest of the stream.
                    _ => {
                        self.esc = EscState::Ground;
                        Typed::Nothing
                    }
                };
            }
        }
        if b == 0x1b {
            self.esc = EscState::Esc;
            Typed::Nothing
        } else {
            Typed::Byte(b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> Vec<Typed> {
        let mut k = Keys::new();
        bytes.iter().map(|&b| k.feed(b)).collect()
    }

    #[test]
    fn sequences_name_their_keys_on_the_final_byte() {
        let cases: &[(&[u8], Key)] = &[
            (b"\x1b[A", Key::Up),
            (b"\x1b[B", Key::Down),
            (b"\x1b[D", Key::Left),
            (b"\x1b[C", Key::Right),
            (b"\x1b[H", Key::Home),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[7~", Key::Home),
            (b"\x1bOH", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1b[4~", Key::End),
            (b"\x1b[8~", Key::End),
            (b"\x1b[3~", Key::Delete),
            (b"\x1b[5~", Key::PageUp),
            (b"\x1b[6~", Key::PageDown),
        ];
        for &(seq, key) in cases {
            let out = feed_all(seq);
            let (last, before) = out.split_last().unwrap();
            assert_eq!(*last, Typed::Key(key), "{seq:?}");
            assert!(before.iter().all(|&t| t == Typed::Nothing), "{seq:?}");
        }
    }

    #[test]
    fn other_bytes_pass() {
        for b in [b'a', b' ', b'~', b'\r', 0x7f] {
            assert_eq!(Keys::new().feed(b), Typed::Byte(b));
        }
    }

    #[test]
    fn a_byte_that_cannot_end_a_sequence_is_acted_on() {
        assert_eq!(feed_all(b"\x1bO\r"), [Typed::Nothing, Typed::Nothing, Typed::Byte(b'\r')]);
        let mut k = Keys::new();
        for &b in b"\x1b[99z" {
            assert_eq!(k.feed(b), Typed::Nothing);
        }
        assert!(!k.in_escape());
        assert_eq!(k.feed(b'x'), Typed::Byte(b'x'));
    }
}
