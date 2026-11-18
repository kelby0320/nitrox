use super::*;
use alloc::string::ToString;
use alloc::vec;
use libline::Keys;

fn numbered(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("line {i}")).collect()
}

/// A pager of `n` numbered lines on a terminal 20 wide and 6 high: five rows of text a page.
fn five_a_page(n: usize) -> Pager {
    Pager::new(&numbered(n), 0, (20, 6))
}

/// Each byte of `typed` through a key decoder, and the last thing the pager said.
fn typed(p: &mut Pager, typed: &[u8]) -> Act {
    let mut keys = Keys::new();
    typed.iter().fold(Act::Nothing, |_, &b| p.feed(keys.feed(b)))
}

#[test]
fn a_long_line_takes_rows_and_an_empty_one_a_row() {
    let lines = vec!["abcdefghij".to_string(), String::new(), "abc".into()];
    let p = Pager::new(&lines, 0, (4, 10));
    assert_eq!(p.page(), ["abcd", "efgh", "ij", "", "abc"]);
    assert!(p.fits());
    assert!(!Pager::new(&lines, 0, (4, 5)).fits(), "five rows, four a page");
}

/// A tab goes to the next stop and every other control is dropped, an escape sequence whole, so a
/// page cannot move the cursor, clear the screen or set a colour, nor show a sequence's tail.
#[test]
fn a_tab_is_spaces_and_no_control_reaches_the_terminal() {
    let lines = vec![
        "a\tb".to_string(),
        "x\x1b[2Jy\r\x07z\u{9b}".into(),
        "\x1b[1;31mred\x1b[0m \u{9b}2Kc".into(),
        "\x1b]0;title\x07t \x1bPq\x1b\\d \x1b(0g \x1b7s \x1b".into(),
    ];
    assert_eq!(Pager::new(&lines, 0, (20, 5)).page(), ["a       b", "xyz", "red c", "t d g s "]);
}

#[test]
fn the_keys_move_by_a_page_and_by_a_line_and_stop_at_the_ends() {
    let mut p = five_a_page(12);
    assert_eq!((typed(&mut p, b" "), p.page()[0]), (Act::Draw, "line 6"));
    assert_eq!((typed(&mut p, b" "), p.page()[0]), (Act::Draw, "line 8"), "the last page, not past it");
    assert_eq!(typed(&mut p, b"\x1b[6~"), Act::Nothing, "Page Down at the end moves nothing");
    assert_eq!((typed(&mut p, b"b"), p.page()[0]), (Act::Draw, "line 3"));
    assert_eq!((typed(&mut p, b"\x1b[5~"), p.page()[0]), (Act::Draw, "line 1"));
    assert_eq!(typed(&mut p, b"k"), Act::Nothing, "a line up at the top moves nothing");
    for (keys, top) in [(&b"j"[..], "line 2"), (b"\r", "line 3"), (b"\x1b[B", "line 4"), (b"\x1b[A", "line 3")] {
        assert_eq!((typed(&mut p, keys), p.page()[0]), (Act::Draw, top), "{keys:?}");
    }
    for (keys, top) in [(&b"G"[..], "line 8"), (b"g", "line 1"), (b"\x1b[F", "line 8"), (b"\x1b[H", "line 1")] {
        assert_eq!((typed(&mut p, keys), p.page()[0]), (Act::Draw, top), "{keys:?}");
    }
    assert_eq!(typed(&mut p, b"x"), Act::Nothing);
    assert_eq!(typed(&mut p, b"q"), Act::Quit);
}

#[test]
fn the_status_says_which_lines_are_shown_and_the_end() {
    let mut p = five_a_page(12);
    assert_eq!(p.status(), "lines 1-5 of 12");
    typed(&mut p, b"G");
    assert_eq!(p.status(), "lines 8-12 of 12 (END)");
}

#[test]
fn a_search_goes_to_the_next_line_holding_its_word() {
    let mut p = five_a_page(30);
    assert_eq!((typed(&mut p, b"/line 3"), p.status()), (Act::Draw, "/line 3".to_string()));
    assert_eq!(typed(&mut p, b"\x7f2\r"), Act::Draw, "Backspace edits the word");
    assert_eq!(p.page()[0], "line 2", "past the top line");
    for want in ["line 20", "line 21", "line 22", "line 23", "line 24", "line 25", "line 26"] {
        typed(&mut p, b"n");
        assert_eq!(p.page()[0], want, "not line 12, which holds no `line 2`");
    }
    // Lines 27 to 29 are on the last page already: `n` finds them where they are, then misses.
    for _ in 0..3 {
        assert_eq!(typed(&mut p, b"n"), Act::Nothing);
    }
    assert_eq!((p.page()[0], p.status()), ("line 26", "lines 26-30 of 30 (END)".to_string()));
    assert_eq!(typed(&mut p, b"n"), Act::Draw);
    assert_eq!(p.status(), "not found: line 2", "said once");
    assert_eq!((typed(&mut p, b"x"), p.status()), (Act::Draw, "lines 26-30 of 30 (END)".to_string()));
}

/// `n` searches on from the page shown: from the line last found only while the page shows it.
#[test]
fn n_searches_on_from_the_page_shown() {
    let lines: Vec<String> =
        (1..=60).map(|i| if i == 11 || i == 31 { format!("x {i}") } else { format!("line {i}") }).collect();
    let mut p = Pager::new(&lines, 0, (20, 6));
    typed(&mut p, b"/x\rn");
    assert_eq!(p.page()[0], "x 31");
    typed(&mut p, b"gn");
    assert_eq!(p.page()[0], "x 11", "on from the top, not from line 31");
    typed(&mut p, b"ngjjjjjjj");
    assert_eq!(p.page(), ["line 8", "line 9", "line 10", "x 11", "line 12"]);
    assert_eq!((typed(&mut p, b"n"), p.status()), (Act::Draw, "lines 11-15 of 60".to_string()), "x 11 is on the page");
}

/// Enter on an empty search runs the last word again; with none, it finds nothing and moves nothing.
#[test]
fn an_empty_search_runs_the_last_word_again() {
    let mut p = five_a_page(30);
    typed(&mut p, b"/\r");
    assert_eq!(p.status(), "lines 1-5 of 30");
    typed(&mut p, b"/line 2\r/\r");
    assert_eq!(p.page()[0], "line 20");
}

#[test]
fn backspace_at_an_empty_search_leaves_it() {
    let mut p = five_a_page(30);
    typed(&mut p, b"/\x7f");
    assert_eq!(p.status(), "lines 1-5 of 30");
    assert_eq!(typed(&mut p, b"q"), Act::Quit, "a key again, not a search's");
}

/// A refit breaks the rows again and keeps the line at the top there, or as near as fills a page.
#[test]
fn a_refit_keeps_the_top_line() {
    let mut p = Pager::new(&numbered(40), 0, (20, 6));
    typed(&mut p, b"  ");
    assert_eq!(p.page()[0], "line 11");
    p.resize((4, 10));
    assert_eq!((p.page()[0], p.page()[1], p.size()), ("line", " 11", (4, 10)));
    let mut p = five_a_page(12);
    typed(&mut p, b"G");
    p.resize((20, 10));
    assert_eq!((p.page().len(), p.status()), (9, "lines 4-12 of 12 (END)".to_string()), "a full page, not 8-12");
}

#[test]
fn a_page_is_drawn_row_by_row_its_header_painted_and_its_status_short_of_the_corner() {
    let lines = vec!["name".to_string(), "a".into(), "b".into()];
    let out = String::from_utf8(Pager::new(&lines, 1, (6, 3)).draw()).unwrap();
    let header = format!("\x1b[1;1H\x1b[K{}name\x1b[0m", libstream::display::HEADER);
    assert!(out.starts_with(&header), "{out:?}");
    assert!(out.contains("\x1b[2;1H\x1b[Ka"), "{out:?}");
    assert!(out.ends_with("\x1b[3;1H\x1b[K\x1b[7mlines\x1b[0m"), "five of the six columns: {out:?}");
}

mod on_a_terminal {
    use super::super::terminal::page_on;
    use super::*;
    use crate::stage::Stage;
    use core::cell::Cell;
    use libkern::abi::KIND_TERMINATE_REQUESTED;
    use libkern::{KError, Notification, Rights};
    use libos::boot::Start;
    use libos::{Carried, Channel, Deadline, Handle, Msg, Only, mock};
    use libprompt::ipc::receive;
    use librsproto::tty::Size;
    use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ, OP_TTY_SIZE, OP_TTY_WRITE, RS_FLAG_ERROR};
    use libstream::setup::{StageStreams, bootstrap_arg0, send_stage_setup};
    use libstream::wire::Record;

    type End = Handle<Channel, Only>;

    fn put(to: &End, op: u16, id: u64, flags: u32, body: &[u8]) {
        let mut m = Msg::new();
        let n = librsproto::encode(m.payload_mut(), op, id, flags, body, 0).unwrap();
        m.set_len(n).unwrap();
        to.send(&m, &mut Carried::new()).unwrap();
    }

    /// The terminal's side: a size of 20 by 6, then `answers` in order, each request answered.
    fn terminal(answers: &[(u16, &[u8])]) -> (End, End) {
        mock::reset();
        let (ours, theirs) = End::pair(32).unwrap();
        put(&theirs, OP_TTY_SIZE, 1, 0, &Size { cols: 20, rows: 6 }.encode());
        for &(op, body) in answers {
            put(&theirs, op, if op == OP_TTY_INTERRUPT { 0 } else { 1 }, 0, body);
        }
        (ours, theirs)
    }

    /// Every request the pager sent, all it wrote as one string, and each read's body.
    fn sent(theirs: &End) -> (Vec<u16>, String, Vec<Vec<u8>>) {
        let (mut ops, mut written, mut reads) = (Vec::new(), String::new(), Vec::new());
        while let Ok(Some((op, _, _, body))) = receive(theirs) {
            ops.push(op);
            if op == OP_TTY_WRITE {
                written.push_str(&String::from_utf8_lossy(&body));
            } else if op == OP_TTY_READ {
                reads.push(body);
            }
        }
        (ops, written, reads)
    }

    /// A stage the shell handed `term`, saying whether its output is `shown`, with `queued`
    /// notifications; and the shell's end of its setup channel.
    fn stage_on(term: End, shown: bool, queued: &[u32]) -> (Stage, End) {
        let (shell, endpoint) = End::pair(4).unwrap();
        let streams = StageStreams { shown, ..StageStreams::default() };
        send_stage_setup(&shell, streams, Some(term), &["page"], &Record::default(), Deadline::NEVER).unwrap();
        let notes: Vec<Notification> = queued
            .iter()
            .map(|&kind| {
                let mut n = Notification::zeroed();
                n.kind = kind;
                n
            })
            .collect();
        let root = mock::with_rights(mock::served("/x"), Rights::LOOKUP);
        let notify = mock::notify(&notes);
        // SAFETY: handles of the model's, each wrapped here and nowhere else.
        let start = unsafe { Start::from_registers(notify, root, endpoint.into_raw().0, bootstrap_arg0(true)) };
        (Stage::enter(start), shell)
    }

    const SIZE: (u16, &[u8]) = (OP_TTY_SIZE, &[20, 0, 6, 0]);
    const ACK: (u16, &[u8]) = (0, &[]);

    #[test]
    fn a_page_is_shown_on_the_alternate_screen_until_q() {
        let (ours, theirs) = terminal(&[ACK, ACK, (OP_TTY_READ, b" "), SIZE, ACK, (OP_TTY_READ, b"q"), SIZE, ACK]);
        assert_eq!(page_on(&ours, None, &|| false, &numbered(12), 0), Shown::Paged);
        let (ops, written, reads) = sent(&theirs);
        let (s, w, r) = (OP_TTY_SIZE, OP_TTY_WRITE, OP_TTY_READ);
        assert_eq!(ops, [s, w, w, r, s, w, r, s, w]);
        assert_eq!(reads, [[1], [1]], "a byte at a time, so what follows `q` stays the shell's");
        assert!(written.starts_with("\x1b[?1049h") && written.ends_with("\x1b[?1049l"), "{written:?}");
        let (first, second) = (written.find("line 1\x1b").unwrap(), written.find("line 6").unwrap());
        assert!(first < second, "the first page, then the second");
    }

    /// A size that changed by the next key draws the page again at it.
    #[test]
    fn a_refit_is_drawn_at_the_next_key() {
        let taller: (u16, &[u8]) = (OP_TTY_SIZE, &[20, 0, 10, 0]);
        let (ours, theirs) = terminal(&[ACK, ACK, (OP_TTY_READ, b"x"), taller, ACK, (OP_TTY_READ, b"q"), taller, ACK]);
        let _ = page_on(&ours, None, &|| false, &numbered(30), 0);
        let written = sent(&theirs).1;
        assert!(!written[..written.find("line 5").unwrap()].contains("\x1b[10;1H"), "first at six rows");
        assert!(written.contains("\x1b[10;1H\x1b[K\x1b[7mlines 1-9 of 30"), "then at ten: {written:?}");
    }

    /// `Ctrl-C`, as an `Interrupt` or as a read answered empty, leaves the pager as `q` does.
    #[test]
    fn ctrl_c_or_a_failing_terminal_leaves_the_alternate_screen() {
        let refusal = (KError::NoAccess as i32).to_le_bytes();
        for (why, end) in [("interrupt", (OP_TTY_INTERRUPT, &[][..])), ("empty read", (OP_TTY_READ, &[][..]))] {
            let (ours, theirs) = terminal(&[ACK, ACK, end, ACK]);
            assert_eq!(page_on(&ours, None, &|| false, &numbered(12), 0), Shown::Paged);
            assert!(sent(&theirs).1.ends_with("\x1b[?1049l"), "{why}");
        }
        let (ours, theirs) = terminal(&[ACK, ACK]);
        put(&theirs, OP_TTY_READ, 1, RS_FLAG_ERROR, &refusal);
        put(&theirs, 0, 1, 0, b"");
        assert_eq!(page_on(&ours, None, &|| false, &numbered(12), 0), Shown::Paged);
        assert!(sent(&theirs).1.ends_with("\x1b[?1049l"), "a refused read");
    }

    /// Asked to end, the pager leaves as `q` does; another notification leaves it paging. The
    /// terminal's next answers are put when the pager looks, so it waits with none queued.
    #[test]
    fn a_terminate_request_leaves_the_pager_and_nothing_else_does() {
        let (s, w, r) = (OP_TTY_SIZE, OP_TTY_WRITE, OP_TTY_READ);
        for (kind, ops) in [(KIND_TERMINATE_REQUESTED, &[s, w, w, r, w][..]), (7, &[s, w, w, r, s, w])] {
            let (ours, theirs) = terminal(&[ACK, ACK]);
            let (stage, _shell) = stage_on(ours, true, &[7, kind]);
            let looked = Cell::new(0);
            let stop = || {
                looked.set(looked.get() + 1);
                let asked = stage.terminate_requested();
                if !asked {
                    put(&theirs, OP_TTY_READ, 1, 0, b"q");
                    put(&theirs, OP_TTY_SIZE, 1, 0, &Size { cols: 20, rows: 6 }.encode());
                }
                put(&theirs, 0, 1, 0, b"");
                asked
            };
            let term = stage.terminal.as_ref().expect("handed one");
            assert_eq!(page_on(term, stage.notif.as_ref(), &stop, &numbered(12), 0), Shown::Paged);
            let (sent_ops, written, _) = sent(&theirs);
            assert_eq!((looked.get(), &sent_ops[..]), (1, ops), "notification {kind}");
            assert!(written.ends_with("\x1b[?1049l"), "notification {kind}: {written:?}");
        }
    }

    /// `show` pages only where the shell said the stage's output is shown; elsewhere it leaves the
    /// terminal alone and the caller writes its output.
    #[test]
    fn a_stage_pages_only_where_its_output_is_shown() {
        let (ours, theirs) = terminal(&[]);
        let (stage, _shell) = stage_on(ours, false, &[]);
        assert_eq!(show(&stage, &numbered(12), 0), Shown::Not);
        assert_eq!(sent(&theirs).0, [], "not even its size asked");
        let (ours, theirs) = terminal(&[ACK, ACK, (OP_TTY_READ, b"q"), SIZE, ACK]);
        let (stage, _shell) = stage_on(ours, true, &[]);
        assert_eq!(show(&stage, &numbered(12), 0), Shown::Paged);
        assert!(sent(&theirs).1.ends_with("\x1b[?1049l"));
    }

    #[test]
    fn what_one_page_holds_is_not_paged() {
        let (ours, theirs) = terminal(&[]);
        assert_eq!(page_on(&ours, None, &|| false, &numbered(5), 0), Shown::Not);
        assert_eq!(sent(&theirs).0, [OP_TTY_SIZE], "asked its size, and nothing more");
    }
}
