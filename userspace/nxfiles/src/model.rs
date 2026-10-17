//! What the browser shows: entries, drives, panes, and the operations on them.

use super::*;

/// A drive the sidebar lists: a filesystem the storage service mounted under `/storage`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// The name it is mounted under, `/storage/<name>` — the table's `mounted` column, never its
    /// `label`: a filesystem with no label is mounted under its partition's name. What `Eject`
    /// names.
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

/// The drives a storage table lists: every row mounted under `/storage`, in the table's order.
/// `init`'s mounts are elsewhere and are not drives; nor is a device with nothing mounted. A
/// table without the columns this needs lists nothing.
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
/// Cut, copy and paste are [`Msg`]s of their own, not actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Make an empty file, named by a prompt.
    NewFile,
    /// Make a directory, named by a prompt.
    NewFolder,
    /// Give the selected entry a new name, from a prompt.
    Rename,
    /// Duplicate the selected entry under a name from a prompt; Copy puts a path on the clipboard.
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
            Action::Delete | Action::Properties => None,
        }
    }

    /// Whether this acts on the selected entry rather than on the directory.
    pub fn needs_selection(self) -> bool {
        matches!(self, Action::Rename | Action::Duplicate | Action::Delete | Action::Properties)
    }
}

/// What an operation was chosen **for**, resolved when it was chosen.
///
/// Every operation is two gestures — a menu choice, then an answer — and the compositor has no
/// input-exclusive window (`TODO(dialog-modality)`), so the path and the selection can move in
/// between: a delete answered after walking into another directory would remove a same-named
/// file the person was never asked about. Resolving here makes the operation mean what it meant
/// when asked for. `show` dropping the prompt covers a listing; only this covers a click that
/// re-selects.
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
/// The outbox shape: `update` is a function of values, and `main`, which holds `/home`, does the
/// syscall itself.
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
        /// Whether it is a directory, which decides whether the copy descends (`copy_file` fails).
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
    /// A rename, not a copy-and-delete: within one filesystem it cannot half-happen.
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
/// Not [`OwnedEntry`], the wire's shape: its own type lets `update` and `view` be tested without
/// a filesystem.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The entry's name, with no path in it.
    pub name: String,
    /// Whether pressing this row descends into it.
    pub is_dir: bool,
    /// Modification time, seconds since the Unix epoch; `0` when the server does not report one.
    pub mtime: i64,
    /// Byte size; `0` for a directory or when unreported.
    pub size: u64,
}

/// The browser's rows sort by `libfs`'s comparison, not one of their own; see [`libfs::Listed`].
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

    /// What the row shows: directories carry a trailing separator, the whole of the marking.
    pub fn label(&self) -> String {
        let mut s = self.name.clone();
        if self.is_dir {
            s.push('/');
        }
        s
    }
}

/// One tab: a directory, what is in it, and where the person is in that list.
///
/// What a person expects to survive switching tabs lives here, scroll offset included; what a
/// *window* has stays on [`App`].
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
    /// How this tab's listing is ordered: per tab, so an order follows you down a tree and a new
    /// tab starts at the default.
    pub(super) order: libfs::Order,
    /// Whether the entries a leading dot marks are listed; per tab like `order`, off by default.
    pub(super) show_hidden: bool,
    /// Moved while it was not current, so what it holds is not where it is: read again when its tab
    /// is chosen. Only the current pane is listed, and a drive going takes its tabs home.
    pub(super) relist: bool,
}
