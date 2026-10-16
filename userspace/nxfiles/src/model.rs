use super::*;

/// A drive the sidebar lists (Phase 6 Part F): a filesystem the storage service mounted under
/// `/storage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// **The name it is mounted under**, `/storage/<name>` — the table's `mounted` column, never
    /// its `label`: a filesystem with no label is mounted under its partition's name, and the
    /// second of two labelled alike under `<label>-2`. It is what `Eject` names.
    pub name: String,
    /// The block device it is on: `n` of the table's `blk-<n>`, and its row's identity.
    pub block: u64,
    /// Whether it is on a removable disk, and so has an eject button.
    pub removable: bool,
}

impl Drive {
    /// Where it is: `/storage/<name>`.
    pub fn path(&self) -> String {
        alloc::format!("/storage/{}", self.name)
    }
}

/// **The drives a storage table lists**: every row mounted under `/storage`, in the table's order.
/// `init`'s mounts are elsewhere and are not drives; nor is a device with nothing mounted.
///
/// A table without the columns this needs lists nothing, which is what a Files given an older
/// service's table should show rather than guess.
pub fn drives(t: &libstream::wire::Table) -> Vec<Drive> {
    use libstream::Value;
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let (Some(name), Some(mounted)) = (col("name"), col("mounted")) else {
        return Vec::new();
    };
    let removable = col("removable");
    t.rows
        .iter()
        .filter_map(|r| {
            let Some(Value::Str(at)) = r.get(mounted) else { return None };
            let mount = at.strip_prefix("/storage/").filter(|m| !m.is_empty() && !m.contains('/'))?;
            let Some(Value::Str(device)) = r.get(name) else { return None };
            let block: u64 = device.strip_prefix("blk-")?.parse().ok().filter(|&n: &u64| n < 1 << 60)?;
            let removable = removable.and_then(|c| r.get(c)) == Some(&Value::Bool(true));
            Some(Drive { name: String::from(mount), block, removable })
        })
        .collect()
}

/// What a menu row asks for.
///
/// **Cut and paste are absent, and the reason is the clipboard.** They are a *pair*, and a pair
/// that holds something between two gestures is a clipboard however it is spelled — so building
/// a private one-slot path buffer here would be a second clipboard shipped weeks before the real
/// one. M12 decision 1 makes the clipboard a resource server precisely so that what you last
/// copied is not readable by everything running, and Part E's own words leave the door open for
/// this: "the type tag exists so a later image or a typed stream is a second kind rather than a
/// second clipboard". A file path is that second kind. **Trigger: Part E's ring exists** —
///
/// Nothing is lost meanwhile: moving a file into a folder is a drag, which is the gesture people
/// reach for first anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Make an empty file, named by a prompt.
    NewFile,
    /// Make a directory, named by a prompt.
    NewFolder,
    /// Give the selected entry a new name, from a prompt.
    Rename,
    /// Duplicate the selected entry under a name from a prompt.
    ///
    /// **Named `Duplicate` since M14 Part D**, when the Edit menu gained a real Copy. They are
    /// different things: this asks for a name and writes a second file beside the first, while
    /// Copy puts a path on the clipboard and does nothing until a Paste.
    Duplicate,
    /// Remove the selected entry, after a confirmation.
    Delete,
    /// Show what is known about the selected entry: where it is, how big, when it changed.
    Properties,
}

impl Action {
    /// What the prompt says before the field, or `None` for the one that asks a question
    /// instead.
    pub fn prompt(self) -> Option<&'static str> {
        match self {
            Action::NewFile => Some("new file:"),
            Action::NewFolder => Some("new folder:"),
            Action::Rename => Some("rename to:"),
            Action::Duplicate => Some("duplicate as:"),
            // Neither of these asks for a name: one asks a question and the other reports.
            Action::Delete | Action::Properties => None,
        }
    }

    /// Whether this acts on the selected entry rather than on the directory.
    pub fn needs_selection(self) -> bool {
        matches!(self, Action::Rename | Action::Duplicate | Action::Delete | Action::Properties)
    }
}

/// What an operation was chosen **for**, resolved at the moment it was chosen.
///
/// **The fix for a whole class of bug** (PR #268 review, blocking 2 and 3). Every operation here
/// is two gestures — choose it from a menu, then answer a prompt or a question — and the first
/// version composed the paths at the *second*, out of `self.path` and `self.list.selected`. Both
/// move while a prompt or a dialog is up: the compositor has no input-exclusive window
/// (`TODO(dialog-modality)`), the prompt is a *keyboard* mode only, and a click on a row still
/// navigates or re-selects. So a delete answered after walking into another directory removed a
/// file there with the same name — one the person was never asked about, while the dialog's own
/// text still named the one they chose.
///
/// Resolving here makes the operation mean what it meant when it was asked for, whatever the
/// window does afterwards.
///
/// **It overlaps with `show` dropping the prompt and the question**, and only partly. A listing
/// is the only thing that changes `self.path`, so for *delete* either fix alone would do; the
/// *selection* moves without one — a click on a file row re-selects and navigates nothing — and
/// there this is the only thing standing between a rename and the wrong file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Target {
    /// The directory the operation happens in, as it was when the row was pressed.
    pub(super) dir: String,
    /// The full path of the entry it acts on — `None` for the two that act on the directory.
    pub(super) from: Option<String>,
    /// That entry's name, which is what the question shows.
    pub(super) name: String,
    /// Whether it is a directory, which decides whether a removal descends.
    pub(super) is_dir: bool,
}

/// A filesystem operation the binary owes, and the browser cannot perform.
///
/// **The outbox shape everything else here uses**: `update` is a function of values and a
/// filesystem mutation is a syscall, so the application says what it wants done and the `main`
/// that owns the namespace does it. `nxfiles` performs these *itself* rather than asking the
/// shell — it holds `/home`, which is the authority these need, and routing them through a
/// supervisor would be asking it to do what the application is already entitled to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileOp {
    /// Create an empty file or a directory at `path`.
    Create {
        /// Where.
        path: String,
        /// A directory rather than a file.
        dir: bool,
    },
    /// Give `from` the name `to`, in the same directory.
    Rename {
        /// The entry as it is.
        from: String,
        /// What it should be called.
        to: String,
    },
    /// Duplicate `from` as `to`.
    Copy {
        /// The entry to read.
        from: String,
        /// The path to write.
        to: String,
        /// Whether it is a directory, which decides whether the copy descends.
        ///
        /// **`copy_file` on a folder just fails**, and reported "could not copy it" for a
        /// perfectly ordinary request (PR #268 review, optional 3). `libfs::copy_tree` was
        /// already there.
        dir: bool,
    },
    /// Remove `path`, and everything under it if it is a directory.
    Delete {
        /// What to remove.
        path: String,
        /// Whether it is a directory, which decides whether the removal descends.
        dir: bool,
    },
    /// Move `from` into a directory, arriving at `to`.
    ///
    /// **A rename, not a copy-and-delete**, because within one filesystem that is what a move
    /// is — and it is the one that cannot half-happen.
    MoveInto {
        /// The entry being dragged.
        from: String,
        /// Where it lands, directory and name.
        to: String,
    },
}

/// The window's size in pixels at startup, before any manager places it.
pub const START_SIZE: Size = Size::new(560, 420);

/// One thing in a directory, as the browser thinks of it.
///
/// **Not [`OwnedEntry`]**, which is the wire's shape: a row is a name plus the one bit this
/// application acts on — whether pressing it descends or opens. Keeping the browser's own type
/// is what lets `update` and `view` be tested with three lines of setup instead of a filesystem.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The entry's name, with no path in it.
    pub name: String,
    /// Whether pressing this row descends into it.
    pub is_dir: bool,
    /// Modification time, seconds since the Unix epoch; `0` when the server does not report one.
    ///
    /// **Carried rather than discarded** (M14 decision 3). `OwnedEntry` has had this since the
    /// directory protocol did, and this struct threw it away on the way up — so a browser that
    /// wanted to sort by date had nothing to sort by, and the information died here rather than
    /// anywhere it could be blamed for.
    pub mtime: i64,
    /// Byte size; `0` for a directory or when unreported.
    ///
    /// Carried for the same reason as `mtime`, and read by Part D's Properties.
    pub size: u64,
}

/// **The browser's rows sort by `libfs`'s comparison, not by one of their own.**
///
/// `Entry` is `OwnedEntry` projected down to what a view needs, so the two shapes differ and the
/// order must not. See [`libfs::Listed`].
impl libfs::Listed for Entry {
    fn sort_name(&self) -> &[u8] {
        self.name.as_bytes()
    }
    fn sort_is_dir(&self) -> bool {
        self.is_dir
    }
    fn sort_mtime(&self) -> i64 {
        self.mtime
    }
}

impl Entry {
    /// A directory entry.
    pub fn dir(name: &str) -> Self {
        Self { name: String::from(name), is_dir: true, mtime: 0, size: 0 }
    }

    /// A file entry.
    pub fn file(name: &str) -> Self {
        Self { name: String::from(name), is_dir: false, mtime: 0, size: 0 }
    }

    /// What the row shows: directories carry a trailing separator, which is the whole of the
    /// marking.
    ///
    /// **A character rather than a colour or an icon**, because the toolkit has no icons and a
    /// colour would be the theme's to change (M11). A trailing `/` is what every terminal
    /// listing in this system already uses, so it is the marking a person has seen before.
    pub fn label(&self) -> String {
        let mut s = self.name.clone();
        if self.is_dir {
            s.push('/');
        }
        s
    }
}

/// Everything the browser is.
/// One tab: a directory, what is in it, and where the person is in that list.
///
/// **Split out in M12 Part D**, the same line `nxedit` draws between a buffer and its window.
/// What stays on [`App`] is what a *window* has; what moves here is what a person expects to
/// survive switching tabs — including the scroll offset, because a tab that jumped back to the
/// top when you came back to it would lose your place every time you looked at another folder.
pub struct Pane {
    /// Identity across frames and across the strip. Not the index: closing a tab renumbers the
    /// rest.
    pub(super) key: u64,
    /// Where the listing came from, absolute and with no trailing separator except at the root.
    pub(super) path: String,
    /// What is in it, sorted.
    pub(super) entries: Vec<Entry>,
    /// Which row is selected and how far the list is scrolled.
    pub(super) list: ListState,
    /// How this tab's listing is ordered.
    ///
    /// **Per tab, beside `path` and `list`**, because it is a property of *this view of this
    /// directory* rather than of the browser. Navigation keeps the pane, so an order chosen once
    /// follows you down a tree; a new tab starts at the default, which is the same rule Part B
    /// settled for a new window — a second instance, not a second view of the first.
    pub(super) order: libfs::Order,
    /// Whether the entries a leading dot marks are listed.
    ///
    /// Per tab for the reason `order` is, and **off by default**: a dot is a convention meaning
    /// "not part of what this directory is for", and a browser that ignored it would put a
    /// person's configuration in front of them every time they opened their home.
    pub(super) show_hidden: bool,
    /// **Moved while it was not current**, so what it holds is not where it is: read again when
    /// its tab is chosen. A drive going takes its tabs home (Phase 6 Part F), and a listing is
    /// read for the current pane alone.
    pub(super) relist: bool,
}
