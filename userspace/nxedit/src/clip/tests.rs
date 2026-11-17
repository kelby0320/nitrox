use super::*;

fn with(text: &str) -> App {
    let mut a = App::new("/home/notes.txt", "/home");
    a.loaded(text, alloc::format!("{text}\n").as_bytes());
    let end = text.len();
    a.buf_mut().text.select_range((0, 0), (0, end));
    a
}

#[test]
fn a_selection_the_clipboard_cannot_hold_is_neither_cut_nor_copied() {
    let max = librsproto::clipboard::MAX_CLIP_BYTES;
    for (n, takes) in [(max, true), (max + 1, false)] {
        for msg in [Msg::Cut, Msg::Copy] {
            let mut a = with(&"x".repeat(n));
            a.update(msg.clone());
            assert_eq!(a.take_clip_request().is_some(), takes, "{n} bytes, {msg:?}: asked the clipboard");
            let left = a.buf().text.text().len();
            assert_eq!(left, if takes && msg == Msg::Cut { 0 } else { n }, "{n} bytes, {msg:?}: what is left");
            if !takes {
                assert!(a.status.starts_with("too long to"), "{n} bytes, {msg:?}: {:?}", a.status);
            }
        }
    }
}

#[test]
fn a_cut_the_clipboard_refuses_is_undone() {
    let mut a = with("hello");
    a.update(Msg::Cut);
    assert_eq!(a.buf().text.text(), "", "precondition: the cut deleted");
    a.copy_refused();
    assert_eq!(a.buf().text.text(), "hello", "undone");
    assert_eq!(a.status, "the clipboard refused the cut, so it is undone");
}

#[test]
fn a_refused_cut_undoes_nothing_typed_after_it() {
    let mut a = with("hello");
    a.update(Msg::Cut);
    a.buf_mut().text.insert('z');
    a.copy_refused();
    assert_eq!(a.buf().text.text(), "z", "the typing stays, and the cut with it");
    assert_eq!(a.status, "the clipboard refused the copy");

    let mut a = with("hello");
    a.update(Msg::Copy);
    a.copy_refused();
    assert_eq!(a.buf().text.text(), "hello", "a refused copy undoes nothing");
    assert_eq!(a.status, "the clipboard refused the copy");

    // A cut the clipboard took, then a copy it refused: the copy has nothing to undo.
    let mut a = with("hello world");
    a.buf_mut().text.select_range((0, 6), (0, 11));
    a.update(Msg::Cut);
    a.buf_mut().text.select_range((0, 0), (0, 5));
    a.update(Msg::Copy);
    a.copy_refused();
    assert_eq!(a.buf().text.text(), "hello ", "the earlier cut stays");
}
