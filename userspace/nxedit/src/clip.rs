//! What a copy or a cut asks of the clipboard, and what the editor does when it is refused.

use super::*;

impl App {
    /// Copy the selection onto the ring, deleting it as well when `cut`. Nothing selected is not a
    /// request: an empty entry would move the ring's serial under a client mid-cycle.
    ///
    /// **A cut deletes before the clipboard has answered**: one longer than the clipboard holds is
    /// refused here, before anything is deleted, and one refused later is undone
    /// ([`copy_refused`](Self::copy_refused)).
    pub(crate) fn copy(&mut self, cut: bool) {
        let Some(text) = self.buf().text.selected_text() else {
            self.status = String::from(if cut { "nothing to cut" } else { "nothing to copy" });
            return;
        };
        let (verb, max) = (if cut { "cut" } else { "copy" }, librsproto::clipboard::MAX_CLIP_BYTES);
        if text.len() > max {
            self.status = alloc::format!("too long to {verb}: {} bytes, and the clipboard holds {max}", text.len());
            return;
        }
        self.cut_at = None;
        if cut {
            self.buf_mut().text.cut_selection();
            self.cut_at = Some((self.current, self.buf().text.revision()));
        }
        self.status = String::from(if cut { "cut" } else { "copied" });
        self.clip_request = Some(ClipRequest::Copy(text));
    }

    /// The clipboard refused what [`copy`](Self::copy) asked of it. A cut is undone, if nothing has
    /// been edited since, so the text it deleted is not lost with nowhere holding it.
    pub fn copy_refused(&mut self) {
        let standing = self.cut_at.take().is_some_and(|c| c == (self.current, self.buf().text.revision()));
        let undone = standing && self.buf_mut().text.undo();
        self.status = String::from(if undone {
            "the clipboard refused the cut, so it is undone"
        } else {
            "the clipboard refused the copy"
        });
    }
}

#[cfg(test)]
mod tests;
