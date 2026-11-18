//! The pager on a stage's terminal: the alternate screen, echo off, and the keys read raw.

use super::{Act, Pager, SIZE_UNKNOWN};
use crate::stage::Stage;
use alloc::string::String;
use alloc::vec::Vec;
use libline::Keys;
use libos::{Carried, Channel, Deadline, Handle, Notify, Only, WaitSet};
use libprompt::ipc::{ready, receive, transmit};
use librsproto::{OP_TTY_INTERRUPT, OP_TTY_READ};

/// What [`show`] did with the lines.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Shown {
    /// Paged on the terminal: the caller writes nothing.
    Paged,
    /// Not paged, the output not shown, no terminal, or one page holding it: written as ever.
    Not,
}

/// Page `lines`, the first `head` a header, on the terminal `stage` was handed, if its output is
/// shown there (`pipeline-stdio.md` § Payload) and one page cannot hold it.
pub fn show(stage: &Stage, lines: &[String], head: usize) -> Shown {
    match &stage.terminal {
        Some(term) if stage.streams.shown => {
            page_on(term, stage.notif.as_ref(), &|| stage.terminate_requested(), lines, head)
        }
        _ => Shown::Not,
    }
}

fn size(term: &Handle<Channel, Only>) -> (usize, usize) {
    libprompt::size_on(term).map_or(SIZE_UNKNOWN, |s| (usize::from(s.cols), usize::from(s.rows)))
}

/// [`show`] on `term`, waking on `notify` to ask `stop` whether the stage is asked to end.
pub(super) fn page_on(
    term: &Handle<Channel, Only>,
    notify: Option<&Handle<Notify, Only>>,
    stop: &dyn Fn() -> bool,
    lines: &[String],
    head: usize,
) -> Shown {
    let mut pager = Pager::new(lines, head, size(term));
    if pager.fits() {
        return Shown::Not;
    }
    // Echo off, or the keys would be drawn over the page.
    libprompt::set_echo_on(term, false);
    let (mut keys, mut reading, mut draw) = (Keys::new(), false, true);
    let _ = libprompt::write_on(term, b"\x1b[?1049h");
    'paging: loop {
        if draw && !libprompt::write_on(term, &pager.draw()) {
            break;
        }
        let Some(typed) = read(term, notify, stop, &mut reading) else { break };
        // Asked at every key, as the shell asks: a refit is seen when someone next types.
        let now = size(term);
        draw = now != pager.size();
        if draw {
            pager.resize(now);
        }
        for b in typed {
            match pager.feed(keys.feed(b)) {
                Act::Quit => break 'paging,
                Act::Draw => draw = true,
                Act::Nothing => {}
            }
        }
    }
    let _ = libprompt::write_on(term, b"\x1b[?1049l");
    libprompt::set_echo_on(term, true);
    Shown::Paged
}

/// The next bytes typed on `term`, the read sent unless one is outstanding: `None` for `Ctrl-C`,
/// a terminal gone or failing, or `stop` true when `notify` wakes. A read answered empty is a
/// `Ctrl-C` that came while none was outstanding.
fn read(
    term: &Handle<Channel, Only>,
    notify: Option<&Handle<Notify, Only>>,
    stop: &dyn Fn() -> bool,
    reading: &mut bool,
) -> Option<Vec<u8>> {
    if !*reading {
        if !transmit(term, OP_TTY_READ, 1, &[], &mut Carried::new()) {
            return None;
        }
        *reading = true;
    }
    let mut set = WaitSet::<2>::new();
    set.add(term).ok()?;
    if let Some(n) = notify {
        set.add(n).ok()?;
    }
    loop {
        match ready(&set, Deadline::NEVER)? {
            0 => match receive(term) {
                Ok(Some((OP_TTY_INTERRUPT, 0, ..))) => return None,
                Ok(Some((_, 1, err, body))) => {
                    *reading = false;
                    return (!err && !body.is_empty()).then_some(body);
                }
                Ok(_) => {}
                Err(_) => return None,
            },
            _ if stop() => return None,
            _ => {}
        }
    }
}
