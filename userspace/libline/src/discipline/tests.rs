use super::*;

/// Feed a string, returning the completed lines and everything echoed.
fn drive(d: &mut Discipline, input: &str) -> (Vec<alloc::string::String>, alloc::string::String) {
    let mut lines = Vec::new();
    let mut echoed = alloc::string::String::new();
    for b in input.bytes() {
        match d.feed(b) {
            Step::None => {}
            Step::Echo(e) => echoed.push_str(&alloc::string::String::from_utf8_lossy(&e)),
            Step::Line { bytes, echo } => {
                lines.push(alloc::string::String::from_utf8_lossy(&bytes).into_owned());
                echoed.push_str(&alloc::string::String::from_utf8_lossy(&echo));
            }
            // `drive` feeds ordinary input; a test that wants EOF asserts on `feed`.
            Step::Eof => panic!("unexpected end-of-input"),
            Step::Key(k) => panic!("unexpected key {k:?}"),
        }
    }
    (lines, echoed)
}

#[test]
fn a_typed_line_is_returned_and_echoed() {
    let mut d = Discipline::new();
    let (lines, echoed) = drive(&mut d, "hello\n");
    assert_eq!(lines, ["hello"]);
    assert_eq!(echoed, "hello\r\n");
}

/// Without the echoed newline the next prompt lands against the input: `alicepassword:`.
#[test]
fn the_terminating_newline_is_echoed() {
    let mut d = Discipline::new();
    let (_, echoed) = drive(&mut d, "alice\n");
    assert!(echoed.ends_with("\r\n"), "echo was {echoed:?}");
}

#[test]
fn cr_and_lf_both_end_a_line() {
    let mut d = Discipline::new();
    assert_eq!(drive(&mut d, "a\r").0, ["a"]);
    assert_eq!(drive(&mut d, "b\n").0, ["b"]);
}

#[test]
fn no_echo_shows_nothing_at_all() {
    let mut d = Discipline::new();
    d.set_echo(false);
    let (lines, echoed) = drive(&mut d, "hunter2\n");
    assert_eq!(lines, ["hunter2"], "the line still reaches the caller");
    assert_eq!(echoed, "", "and nothing whatever was shown");
}

#[test]
fn backspace_erases_one_character() {
    let mut d = Discipline::new();
    let (lines, echoed) = drive(&mut d, "abc\x08\n");
    assert_eq!(lines, ["ab"]);
    assert!(echoed.contains("\x08 \x08"));
}

#[test]
fn del_erases_like_backspace() {
    let mut d = Discipline::new();
    assert_eq!(drive(&mut d, "abc\x7f\n").0, ["ab"]);
}

#[test]
fn backspace_at_an_empty_line_writes_nothing() {
    let mut d = Discipline::new();
    let (_, echoed) = drive(&mut d, "\x08");
    assert_eq!(echoed, "");
}

#[test]
fn ctrl_u_kills_the_line() {
    let mut d = Discipline::new();
    let (lines, _) = drive(&mut d, "throw away\x15kept\n");
    assert_eq!(lines, ["kept"]);
}

#[test]
fn killing_a_hidden_line_erases_nothing_on_screen() {
    let mut d = Discipline::new();
    d.set_echo(false);
    let (_, echoed) = drive(&mut d, "secret\x15\n");
    assert_eq!(echoed, "");
}

#[test]
fn ctrl_w_erases_a_word_and_the_spaces_before_it() {
    let mut d = Discipline::new();
    assert_eq!(drive(&mut d, "list /bin\x17\n").0, ["list "]);
    assert_eq!(drive(&mut d, "one two   \x17\n").0, ["one "]);
}

#[test]
fn an_over_long_line_is_refused_not_truncated() {
    let mut d = Discipline::new();
    for _ in 0..LINE_MAX {
        d.feed(b'x');
    }
    assert_eq!(d.feed(b'y'), Step::None, "further input is refused");
    match d.feed(b'\n') {
        Step::Line { bytes, .. } => {
            assert_eq!(bytes.len(), LINE_MAX);
            assert!(bytes.iter().all(|&b| b == b'x'), "no `y` sneaked in");
        }
        other => panic!("expected a line, got {other:?}"),
    }
}

#[test]
fn ctrl_d_at_an_empty_line_is_end_of_input() {
    let mut d = Discipline::new();
    assert_eq!(d.feed(0x04), Step::Eof);
}

#[test]
fn ctrl_d_mid_line_does_nothing() {
    let mut d = Discipline::new();
    d.feed(b'a');
    assert_eq!(d.feed(0x04), Step::None);
    match d.feed(b'\n') {
        Step::Line { bytes, .. } => assert_eq!(bytes, b"a"),
        other => panic!("expected the line intact, got {other:?}"),
    }
}

#[test]
fn an_empty_line_is_not_end_of_input() {
    let mut d = Discipline::new();
    match d.feed(b'\n') {
        Step::Line { bytes, .. } => assert!(bytes.is_empty()),
        other => panic!("expected an empty line, got {other:?}"),
    }
}

#[test]
fn cursor_keys_are_reported_not_typed() {
    let mut d = Discipline::new();
    assert_eq!(d.feed(0x1b), Step::None);
    assert_eq!(d.feed(b'['), Step::None);
    assert_eq!(d.feed(b'A'), Step::Key(Key::Up));
    assert_eq!(d.feed(0x1b), Step::None);
    assert_eq!(d.feed(b'['), Step::None);
    assert_eq!(d.feed(b'B'), Step::Key(Key::Down));
    match d.feed(b'\n') {
        Step::Line { bytes, .. } => assert!(bytes.is_empty(), "got {bytes:?}"),
        other => panic!("expected an empty line, got {other:?}"),
    }
}

#[test]
fn an_unknown_escape_sequence_leaks_nothing() {
    let mut d = Discipline::new();
    for b in b"ab\x1b[Zcd\n" {
        if let Step::Line { bytes, .. } = d.feed(*b) {
            assert_eq!(bytes, b"abcd");
            return;
        }
    }
    panic!("no line produced");
}

/// `ESC x` is how a terminal sends Alt-x; the discipline does not act on it.
#[test]
fn a_bare_escape_consumes_exactly_two_bytes() {
    let mut d = Discipline::new();
    d.feed(0x1b);
    match d.feed(b'x') {
        // ESC-then-`x` is a two-byte sequence we do not recognise; `x` ends it.
        Step::None => {}
        other => panic!("expected the sequence to end, got {other:?}"),
    }
    assert!(d.line().is_empty());
    // And the byte after those two is ordinary text again.
    d.feed(b'y');
    assert_eq!(d.line(), b"y");
}

#[test]
fn a_parameterised_sequence_leaves_nothing_behind() {
    // Ending `ESC [ 3 ~` at the `3` would leave the `~` to be typed and echoed.
    for seq in [
        &b"\x1b[3~"[..],   // Delete
        &b"\x1b[2~"[..],   // Insert
        &b"\x1b[5~"[..],   // PageUp
        &b"\x1b[6~"[..],   // PageDown
        &b"\x1b[H"[..],    // Home
        &b"\x1b[F"[..],    // End
        &b"\x1b[1;5D"[..], // a modified cursor key: two parameters and a separator
    ] {
        let mut d = Discipline::new();
        for b in seq {
            d.feed(*b);
        }
        for b in b"hi" {
            d.feed(*b);
        }
        assert_eq!(d.line(), b"hi", "{seq:x?} leaked into the line");
    }
}

#[test]
fn the_arrows_still_arrive_after_the_widening() {
    for (seq, key) in [(&b"\x1b[A"[..], Key::Up), (&b"\x1b[B"[..], Key::Down)] {
        let mut d = Discipline::new();
        let mut got = None;
        for b in seq {
            if let Step::Key(k) = d.feed(*b) {
                got = Some(k);
            }
        }
        assert_eq!(got, Some(key), "{seq:x?}");
    }
}

#[test]
fn an_escape_inside_a_sequence_starts_a_new_one() {
    // A program interrupted mid-sequence emits a fresh one; its `ESC` is not payload.
    let mut d = Discipline::new();
    let mut got = None;
    for b in b"\x1b[1\x1b[Ahi" {
        if let Step::Key(k) = d.feed(*b) {
            got = Some(k);
        }
    }
    assert_eq!(got, Some(Key::Up), "the second sequence was eaten by the first");
    assert_eq!(d.line(), b"hi");
}

#[test]
fn a_malformed_csi_does_not_swallow_the_stream() {
    let mut d = Discipline::new();
    for b in b"\x1b[1\x7fZhi" {
        d.feed(*b);
    }
    // `0x7f` can appear in no CSI, so the sequence is abandoned there and `Z` is text,
    // as in `libterm::parse`.
    assert_eq!(d.line(), b"Zhi");
}

#[test]
fn replacing_a_line_erases_what_was_shown() {
    let mut d = Discipline::new();
    drive(&mut d, "abc");
    let out = d.replace_line(b"xy");
    assert_eq!(out, b"\x08 \x08\x08 \x08\x08 \x08xy");
    assert_eq!(d.line(), b"xy");
}

#[test]
fn replacing_a_hidden_line_writes_nothing() {
    let mut d = Discipline::new();
    d.set_echo(false);
    drive(&mut d, "secret");
    assert_eq!(d.replace_line(b"x"), b"");
    assert_eq!(d.line(), b"x", "the buffer still changed");
}

#[test]
fn a_replaced_line_can_be_edited_and_submitted() {
    let mut d = Discipline::new();
    d.replace_line(b"list /");
    d.feed(0x08); // backspace
    match d.feed(b'\n') {
        Step::Line { bytes, .. } => assert_eq!(bytes, b"list "),
        other => panic!("expected a line, got {other:?}"),
    }
}

#[test]
fn undefined_control_bytes_are_dropped() {
    let mut d = Discipline::new();
    assert_eq!(drive(&mut d, "a\x01\x02b\n").0, ["ab"]);
}

#[test]
fn reset_discards_a_partial_line() {
    let mut d = Discipline::new();
    drive(&mut d, "partial");
    d.reset();
    assert_eq!(drive(&mut d, "fresh\n").0, ["fresh"]);
}

/// Feed `bytes` as `nxsh` does, acting on each reported key, and return what was written.
fn edited(d: &mut Discipline, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in bytes {
        match d.feed(b) {
            Step::Echo(e) => out.extend(e),
            Step::Key(k) => out.extend(d.edit(k)),
            _ => {}
        }
    }
    out
}

const LEFT: &[u8] = b"\x1b[D";

#[test]
fn left_and_right_move_by_a_character_and_stop_at_the_ends() {
    let mut d = Discipline::new();
    edited(&mut d, b"ab");
    assert_eq!(edited(&mut d, LEFT), b"\x08");
    assert_eq!(d.cursor(), 1);
    assert_eq!(edited(&mut d, b"\x1b[D\x1b[D"), b"\x08", "the second Left at the start does nothing");
    assert_eq!(edited(&mut d, b"\x1b[C"), b"a", "Right passes the character by writing it");
    assert_eq!(edited(&mut d, b"\x1b[C\x1b[C"), b"b");
    assert_eq!((d.line(), d.cursor()), (&b"ab"[..], 2));
}

#[test]
fn home_and_end_walk_the_whole_line() {
    let mut d = Discipline::new();
    edited(&mut d, b"abc");
    assert_eq!(edited(&mut d, b"\x1b[H"), b"\x08\x08\x08");
    assert_eq!(d.cursor(), 0);
    assert_eq!(edited(&mut d, b"\x1b[F"), b"abc");
    assert_eq!(d.cursor(), 3);
}

#[test]
fn typing_mid_line_inserts_and_redraws_the_rest() {
    let mut d = Discipline::new();
    edited(&mut d, b"ac");
    edited(&mut d, LEFT);
    assert_eq!(edited(&mut d, b"b"), b"bc\x08");
    assert_eq!((d.line(), d.cursor()), (&b"abc"[..], 2));
    edited(&mut d, b"\x1b[H");
    assert_eq!(edited(&mut d, b"_"), b"_abc\x08\x08\x08", "at the start");
    assert_eq!(d.line(), b"_abc");
}

#[test]
fn backspace_mid_line_takes_the_character_before_the_cursor() {
    let mut d = Discipline::new();
    edited(&mut d, b"abcd");
    edited(&mut d, b"\x1b[D\x1b[D");
    assert_eq!(edited(&mut d, b"\x7f"), b"\x08cd \x08\x08\x08");
    assert_eq!((d.line(), d.cursor()), (&b"acd"[..], 1));
    edited(&mut d, b"\x1b[H");
    assert_eq!(edited(&mut d, b"\x7f"), b"", "nothing before the start");
}

#[test]
fn delete_takes_the_character_under_the_cursor() {
    let mut d = Discipline::new();
    edited(&mut d, b"abc");
    edited(&mut d, b"\x1b[H");
    assert_eq!(edited(&mut d, b"\x1b[3~"), b"bc \x08\x08\x08");
    assert_eq!((d.line(), d.cursor()), (&b"bc"[..], 0));
    edited(&mut d, b"\x1b[F");
    assert_eq!(edited(&mut d, b"\x1b[3~"), b"", "nothing under the cursor at the end");
    assert_eq!(d.line(), b"bc");
}

#[test]
fn ctrl_u_and_ctrl_w_take_only_what_is_before_the_cursor() {
    let mut d = Discipline::new();
    edited(&mut d, b"one two");
    edited(&mut d, b"\x1b[D\x1b[D");
    // The cursor is before `w`, so the word's part before it, `t`, goes.
    assert_eq!(edited(&mut d, b"\x17"), b"\x08wo \x08\x08\x08");
    assert_eq!((d.line(), d.cursor()), (&b"one wo"[..], 4));
    assert_eq!(edited(&mut d, b"\x15"), b"\x08\x08\x08\x08wo    \x08\x08\x08\x08\x08\x08");
    assert_eq!((d.line(), d.cursor()), (&b"wo"[..], 0));
}

#[test]
fn enter_mid_line_submits_it_all_and_the_next_line_starts_empty() {
    let mut d = Discipline::new();
    edited(&mut d, b"abc");
    edited(&mut d, LEFT);
    match d.feed(b'\r') {
        Step::Line { bytes, .. } => assert_eq!(bytes, b"abc"),
        other => panic!("expected a line, got {other:?}"),
    }
    // A cursor left at 2 would insert past the end of the empty line and panic.
    edited(&mut d, b"x");
    assert_eq!((d.line(), d.cursor()), (&b"x"[..], 1));
}

#[test]
fn a_reset_mid_line_leaves_the_cursor_at_the_start() {
    let mut d = Discipline::new();
    edited(&mut d, b"abc");
    edited(&mut d, LEFT);
    d.reset();
    edited(&mut d, b"x");
    assert_eq!((d.line(), d.cursor()), (&b"x"[..], 1));
}

#[test]
fn a_replaced_line_leaves_the_cursor_at_its_end_and_erases_from_there() {
    let mut d = Discipline::new();
    edited(&mut d, b"abc");
    edited(&mut d, b"\x1b[H");
    // The rest is passed over first, so the erase starts from the end of what is shown.
    assert_eq!(d.replace_line(b"xy"), b"abc\x08 \x08\x08 \x08\x08 \x08xy");
    assert_eq!(d.cursor(), 2);
}

#[test]
fn replace_around_puts_the_cursor_between_its_halves() {
    let mut d = Discipline::new();
    assert_eq!(d.replace_around(b"cd /ho", b" x"), b"cd /ho x\x08\x08");
    assert_eq!((d.line(), d.cursor()), (&b"cd /ho x"[..], 6));
    assert_eq!(d.redraw(), b"cd /ho x\x08\x08");
}

#[test]
fn a_recalled_line_too_long_is_cut_at_a_whole_character() {
    let mut d = Discipline::new();
    let mut recalled = alloc::vec![b'a'; LINE_MAX - 1];
    recalled.extend_from_slice("\u{e9}".as_bytes());
    let shown = d.replace_line(&recalled);
    assert_eq!(d.line().len(), LINE_MAX - 1, "the half of `\u{e9}` that fit is dropped");
    assert!(core::str::from_utf8(d.line()).is_ok());
    assert_eq!(shown, d.line(), "only what is kept is written");
}

#[test]
fn the_cursor_moves_over_a_character_not_a_byte() {
    let mut d = Discipline::new();
    d.replace_line("h\u{e9}llo".as_bytes());
    edited(&mut d, b"\x1b[H\x1b[C");
    assert_eq!(d.cursor(), 1);
    assert_eq!(edited(&mut d, b"\x1b[C"), "\u{e9}".as_bytes());
    edited(&mut d, LEFT);
    assert_eq!(edited(&mut d, b"\x1b[3~"), b"llo \x08\x08\x08\x08", "one character, two bytes, gone");
    assert_eq!(d.line(), b"hllo");
}

#[test]
fn a_recalled_statement_of_several_lines_is_edited_on_its_last() {
    let mut d = Discipline::new();
    d.replace_line(b"if x {\n1 }");
    assert_eq!(edited(&mut d, b"\x1b[H"), b"\x08\x08\x08");
    assert_eq!(d.cursor(), 7);
    assert_eq!(edited(&mut d, b"\x1b[D\x7f"), b"", "no Left or Backspace past the row's start");
    assert_eq!(d.line(), b"if x {\n1 }");
}

#[test]
fn every_form_of_each_key_is_reported() {
    for (seq, key) in [
        (&b"\x1b[D"[..], Key::Left),
        (b"\x1b[C", Key::Right),
        (b"\x1b[H", Key::Home),
        (b"\x1b[1~", Key::Home),
        (b"\x1b[7~", Key::Home),
        (b"\x1bOH", Key::Home),
        (b"\x1b[F", Key::End),
        (b"\x1b[4~", Key::End),
        (b"\x1b[8~", Key::End),
        (b"\x1bOF", Key::End),
        (b"\x1b[3~", Key::Delete),
        (b"\x1b[1;5D", Key::Left),
        (b"\x1bOA", Key::Up),
    ] {
        let mut d = Discipline::new();
        let got: Vec<Step> = seq.iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
        assert_eq!(got, [Step::Key(key)], "{seq:x?}");
    }
    // Insert names no key, and leaves nothing.
    let mut d = Discipline::new();
    assert_eq!(edited(&mut d, b"\x1b[2~"), b"");
}

#[test]
fn an_ss3_sequence_ends_at_its_final_byte_rather_than_typing_it() {
    let mut d = Discipline::new();
    edited(&mut d, b"\x1bOxy");
    assert_eq!(d.line(), b"y");
}

#[test]
fn with_echo_off_an_edit_still_moves_the_cursor_but_shows_nothing() {
    let mut d = Discipline::new();
    d.set_echo(false);
    edited(&mut d, b"ab");
    assert_eq!(edited(&mut d, b"\x1b[DX"), b"");
    assert_eq!(d.line(), b"aXb");
}

#[test]
fn ctrl_d_ends_input_only_on_an_empty_line_wherever_the_cursor_is() {
    let mut d = Discipline::new();
    edited(&mut d, b"ab\x1b[H");
    assert_eq!(d.feed(0x04), Step::None, "the cursor at the start of a line is not an empty line");
    assert_eq!(d.line(), b"ab");
}

#[test]
fn ctrl_w_takes_the_word_before_the_cursor_not_the_last() {
    let mut d = Discipline::new();
    edited(&mut d, b"one two three\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D");
    edited(&mut d, b"\x17");
    assert_eq!((d.line(), d.cursor()), (&b"one  three"[..], 4));
}

#[test]
fn ctrl_u_on_a_recalled_statement_takes_only_its_last_row() {
    let mut d = Discipline::new();
    d.replace_line(b"if x {\n1 }");
    edited(&mut d, b"\x15");
    assert_eq!(d.line(), b"if x {\n");
}

#[test]
fn a_reset_abandons_a_half_read_sequence() {
    let mut d = Discipline::new();
    d.feed(0x1b);
    d.reset();
    edited(&mut d, b"x");
    assert_eq!(d.line(), b"x");
}

#[test]
fn a_modifier_after_the_first_parameter_is_read_past() {
    let mut d = Discipline::new();
    let got: Vec<Step> = b"\x1b[3;5~".iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
    assert_eq!(got, [Step::Key(Key::Delete)], "Ctrl-Delete is Delete");
}

#[test]
fn with_echo_off_a_redraw_shows_nothing() {
    let mut d = Discipline::new();
    d.set_echo(false);
    edited(&mut d, b"secret");
    assert_eq!(d.redraw(), b"");
}

#[test]
fn replace_around_never_leaves_the_cursor_before_the_last_row() {
    let mut d = Discipline::new();
    d.replace_around(b"ab", b"c\nd");
    assert_eq!(d.cursor(), 4);
    assert_eq!(edited(&mut d, b"\x1b[H"), b"", "Home stays on the last row");
}

#[test]
fn a_completion_that_would_pass_line_max_mid_line_is_refused_not_cut() {
    let mut d = Discipline::new();
    let line = alloc::vec![b'a'; LINE_MAX];
    d.replace_line(&line);
    assert_eq!(d.replace_around(&line[..4], &line[..LINE_MAX]), b"");
    assert_eq!((d.line().len(), d.cursor()), (LINE_MAX, LINE_MAX), "the line is as it was");
}

#[test]
fn replacing_the_line_abandons_a_half_read_sequence() {
    let mut d = Discipline::new();
    d.feed(0x1b);
    d.replace_line(b"date");
    assert!(matches!(d.feed(b'\n'), Step::Line { .. }), "the Enter is not taken by the `ESC`");
}

#[test]
fn a_byte_that_cannot_end_an_ss3_sequence_is_acted_on() {
    let mut d = Discipline::new();
    edited(&mut d, b"ab\x1bO");
    assert!(matches!(d.feed(b'\r'), Step::Line { bytes, .. } if bytes == b"ab"), "Enter still ends the line");
}

#[test]
fn an_escape_after_ss3_starts_a_new_sequence() {
    let mut d = Discipline::new();
    let got: Vec<Step> = b"\x1bO\x1b[D".iter().map(|&b| d.feed(b)).filter(|s| *s != Step::None).collect();
    assert_eq!(got, [Step::Key(Key::Left)]);
}
