//! What each coreutil says of itself: its usage line and a sentence, and the flags it parses. One
//! table parses a program's command line and prints its `--help`, and its page in the manual is
//! held to it, so a flag is described where it is parsed and nowhere else is left behind.

use crate::args::{self, ArgError, Args, Flag};
use alloc::format;
use alloc::string::String;

/// A program's usage and its flags.
#[derive(Copy, Clone, Debug)]
pub struct Usage {
    /// Its name, as `/bin` has it.
    pub name: &'static str,
    /// Its usage line, then a blank line and a sentence or two: `--help`'s head.
    pub about: &'static str,
    /// The flags it parses, besides `--help` and `--version`.
    pub flags: &'static [Flag],
}

impl Usage {
    /// `argv` parsed against this program's flags.
    pub fn parse(&self, argv: &[String]) -> Result<Args, ArgError> {
        args::parse(argv, self.flags)
    }

    /// What `--help` prints.
    pub fn help(&self) -> String {
        args::help(self.about, self.flags)
    }

    /// What `--version` prints.
    pub fn version(&self) -> String {
        format!("{} (nitrox coreutils) 0.1.0\n", self.name)
    }
}

/// `list`.
pub const LIST: Usage = Usage {
    name: "list",
    about: "usage: list [--recursive] [PATH...]\n\
            \n\
            List each PATH, or the working directory, as a typed stream (TSM1) on stdout:\n\
            Table<{name: String, size: Int, kind: String, modified: Int}>",
    flags: &[Flag::new("recursive", 'r', "list subdirectories recursively")],
};

/// `copy`.
pub const COPY: Usage = Usage {
    name: "copy",
    about: "usage: copy [--force] SOURCE... DEST\n\
            \n\
            Copy files and directories. Directories are copied recursively.\n\
            Emits Table<{source: String, destination: String, bytes: Int}> on stdout.",
    flags: &[Flag::new("force", 'f', "overwrite an existing destination")],
};

/// `mkdir`.
pub const MKDIR: Usage = Usage {
    name: "mkdir",
    about: "usage: mkdir [--parents] PATH...\n\
            \n\
            Create directories.\n\
            Emits Table<{path: String, created: Bool}> on stdout.",
    flags: &[Flag::new(
        "parents",
        'p',
        "create missing parent directories, and treat an existing PATH as success rather than an error",
    )],
};

/// `remove`.
pub const REMOVE: Usage = Usage {
    name: "remove",
    about: "usage: remove [--recursive] [--force] PATH...\n\
            \n\
            Remove files and directories.\n\
            Emits Table<{path: String, kind: String}> on stdout.",
    flags: &[
        Flag::new("recursive", 'r', "remove directories and everything under them"),
        Flag::new("force", 'f', "a path that does not exist is not an error"),
    ],
};

/// `rename`.
pub const RENAME: Usage = Usage {
    name: "rename",
    about: "usage: rename [--force] OLD NEW\n\
            \n\
            Re-point a name at the same file. Never copies: a rename across mounts\n\
            is an error (use `move`, which falls back to copy).\n\
            Emits Table<{from: String, to: String}> on stdout.",
    flags: &[Flag::new("force", 'f', "replace an existing NEW")],
};

/// `move`.
pub const MOVE: Usage = Usage {
    name: "move",
    about: "usage: move [--force] SOURCE... DEST\n\
            \n\
            Relocate files. Re-points the name when source and destination share a\n\
            mount; copies and removes the original when they do not.\n\
            Emits Table<{from: String, to: String, method: String}> on stdout,\n\
            where method is \"rename\" or \"copy\".",
    flags: &[Flag::new("force", 'f', "replace an existing destination")],
};

/// `touch`.
pub const TOUCH: Usage = Usage {
    name: "touch",
    about: "usage: touch [--no-create] PATH...\n\
            \n\
            Create each PATH if absent, and stamp its modification time.\n\
            The time is the filesystem's own clock; it cannot be supplied.\n\
            Emits Table<{path: String, created: Bool}> on stdout.",
    flags: &[Flag::new("no-create", 'c', "stamp only; skip a PATH that does not exist")],
};

/// `date`.
pub const DATE: Usage = Usage {
    name: "date",
    about: "usage: date [--unix]\n\
            \x20      date --set TIME [--unix]\n\
            \n\
            Report the current wall-clock time (UTC).\n\
            Emits Table<{unix: Int, year: Int, month: Int, day: Int,\n\
            \x20            hour: Int, minute: Int, second: Int}> on stdout.\n\
            \n\
            With --set, first set the clock, and the machine's hardware clock, to TIME: UTC,\n\
            as 2026-09-28T14:30:00Z, from 2000 to 2099. That needs the clock grant:\n\
            `with admin date --set TIME`.",
    flags: &[
        Flag::new("unix", 'u', "report the bare epoch value only"),
        Flag::long_only("set", "set the clock to TIME first (needs the clock grant)").with_args("TIME"),
    ],
};

/// `sleep`.
pub const SLEEP: Usage = Usage {
    name: "sleep",
    about: "usage: sleep DURATION\n\
            \n\
            Wait for DURATION, then exit. A bare number is seconds; the suffixes\n\
            ns, us, ms, s, m and h are accepted, with an optional fraction.\n\
            Emits nothing.",
    flags: &[],
};

/// `whoami`.
pub const WHOAMI: Usage = Usage {
    name: "whoami",
    about: "usage: whoami\n\
            \n\
            Report the user this session was opened for, read from /session/user.\n\
            Fails if there is no session (nothing is bound there).\n\
            Emits Table<{user: String}> on stdout.",
    flags: &[],
};

/// `desktop`.
pub const DESKTOP: Usage = Usage {
    name: "desktop",
    about: "usage: desktop [switch N | name N LABEL]\n\
            \n\
            With no operands, list the session's desktops and mark the current one.\n\
            switch N    make the Nth desktop current\n\
            name N L    name the Nth desktop, which is what makes it persist\n\
            \n\
            N is a position as the desktop indicator counts them, starting at 1.",
    flags: &[],
};

/// `clip`.
pub const CLIP: Usage = Usage {
    name: "clip",
    about: "usage: clip [INDEX]\n\
            \x20      clip --copy\n\
            \x20      clip --list\n\
            \n\
            Read and write the clipboard (/dev/clipboard).\n\
            \n\
            With no operand, writes the newest entry to stdout as a text stream.\n\
            INDEX reaches further back: 0 is the newest, 1 the one before it.",
    flags: &[
        Flag::new("copy", 'c', "read stdin and push its text onto the ring"),
        Flag::new("list", 'l', "emit Table<{index: Int, kind: Int, bytes: Int}>, one row per entry"),
    ],
};

/// `with`.
pub const WITH: Usage = Usage {
    name: "with",
    about: "usage: with VIEW PROGRAM [ARG...]\n\
            \x20      with --forget\n\
            \x20      with --list\n\
            \x20      with --check FILE\n\
            \x20      with --show [FILE]\n\
            \x20      with --install FILE\n\
            \n\
            Run PROGRAM in VIEW: this session's namespace plus what the view grants,\n\
            when /system/views.toml says you may. Asks for your password when the\n\
            policy does, and remembers it for five minutes on this terminal, for that\n\
            view. PROGRAM is a bare name; its output goes where with's would.",
    flags: &[
        Flag::long_only("forget", "forget every password this session has remembered"),
        Flag::long_only("list", "the views you may use, as a table"),
        Flag::long_only("check", "whether FILE is a valid policy").with_args("FILE"),
        Flag::long_only("show", "the policy, or a copy of it written to FILE (needs the views grant)")
            .with_args("[FILE]"),
        Flag::long_only("install", "check FILE and install it as the policy (needs the views grant)").with_args("FILE"),
    ],
};

/// `disk`.
pub const DISK: Usage = Usage {
    name: "disk",
    about: "usage: disk --list | --mount DEVICE [LABEL] | --unmount LABEL | --eject NAME\n\
            \x20      | --partition DISK [--mbr|--gpt]\n\
            \x20      | --format DEVICE fat|ext4 [LABEL] [--mbr|--gpt]\n\
            \n\
            The machine's disks and their filesystems. Mounting and unmounting need the\n\
            storage grant, and partitioning and formatting the disks grant too: run them\n\
            with `with admin`. Ejecting a removable drive needs neither.",
    flags: &[
        Flag::long_only(
            "list",
            "every block device, what it holds, where it is mounted, and whether it was left clean",
        ),
        Flag::long_only(
            "mount",
            "mount DEVICE (/dev/blk/N or blk-N) at /storage/LABEL, or under the label the service \
             chooses; read-only, saying why, if its server only reads it",
        )
        .with_args("DEVICE [LABEL]"),
        Flag::long_only(
            "unmount",
            "write back every file, flush the drive, and unmount; refused while a file on it is open",
        )
        .with_args("LABEL"),
        Flag::long_only(
            "eject",
            "the same for every filesystem on the removable drive holding /storage/NAME, after which \
             the drive can be pulled out",
        )
        .with_args("NAME"),
        Flag::long_only(
            "partition",
            "one partition spanning a removable DISK, in an MBR or a GPT, from 2 TiB a GPT; \
             everything on DISK is lost",
        )
        .with_args("DISK"),
        Flag::long_only(
            "format",
            "a fat or ext4 filesystem on DEVICE; on a disk, its table first, an MBR for fat and a GPT \
             for ext4; everything on DEVICE is lost",
        )
        .with_args("DEVICE FS [LABEL]"),
        Flag::long_only("mbr", "with --partition or --format of a disk: an MBR"),
        Flag::long_only("gpt", "with --partition or --format of a disk: a GPT"),
    ],
};

/// `account`.
pub const ACCOUNT: Usage = Usage {
    name: "account",
    about: "usage: account --list\n\
            \x20      account --password\n\
            \x20      account --add NAME\n\
            \x20      account --remove NAME [--home]\n\
            \x20      account --password NAME [--users FILE]\n\
            \n\
            The accounts on this machine. --add, --remove and --password NAME need the\n\
            accounts grant: run them with `with admin`.",
    flags: &[
        Flag::long_only(
            "list",
            "every account, its home, how many sessions it has open, and whether it could administer, \
             as a table",
        ),
        Flag::long_only(
            "add",
            "add NAME, with a home at /home/NAME, asking for its password twice (needs the accounts grant)",
        )
        .with_args("NAME"),
        Flag::long_only(
            "remove",
            "remove NAME, keeping its home unless --home; refused while NAME is logged in, or if nobody \
             left could administer (needs the accounts grant)",
        )
        .with_args("NAME"),
        Flag::long_only("home", "with --remove: remove the account's home too"),
        Flag::long_only(
            "password",
            "change your own password: the current one, then a new one twice; or set NAME's, asking \
             for it twice (needs the accounts grant)",
        )
        .with_args("[NAME]"),
        Flag::long_only(
            "users",
            "with --password NAME: set it in FILE, a users file, directly, for recovery from the live \
             image",
        )
        .with_args("FILE"),
    ],
};

/// `service`.
pub const SERVICE: Usage = Usage {
    name: "service",
    about: "usage: service --list | --start NAME | --stop NAME | --restart NAME\n\
            \n\
            Starting and stopping need the services grant: run them with `with admin`. An\n\
            essential service cannot be stopped or restarted.",
    flags: &[
        Flag::long_only(
            "list",
            "every declared service, whether it is starting, running, stopped or failed, and how often \
             it has restarted, as a table",
        ),
        Flag::long_only("start", "start a service that is not running, and wait until it is up").with_args("NAME"),
        Flag::long_only("stop", "ask a running service to stop, and wait until it has").with_args("NAME"),
        Flag::long_only("restart", "stop the service and start it again").with_args("NAME"),
    ],
};

/// `shutdown`.
pub const SHUTDOWN: Usage = Usage {
    name: "shutdown",
    about: "usage: shutdown [--reboot]\n\
            \n\
            Ask every session and service to stop, write back and unmount every filesystem,\n\
            and stop the machine: with nothing, halt, saying when it is safe to turn it off;\n\
            with --reboot, restart.\n\
            \n\
            It needs the power grant: run it as `with power shutdown`.",
    flags: &[Flag::long_only("reboot", "restart the machine rather than halting it")],
};

/// `log`.
pub const LOG: Usage = Usage {
    name: "log",
    about: "usage: log [PRINCIPAL]\n\
            \x20      log --kernel\n\
            \n\
            Read a log back, oldest first. With no option, the service log: every record\n\
            the logging service still holds, or PRINCIPAL's alone. With --kernel, the\n\
            kernel's log, a row a line.\n\
            Emits Table<{sequence: Int, time: Int?, principal: String, tier: String?,\n\
            \x20            level: String?, message: String}> on stdout. `time` is Unix epoch\n\
            seconds, and null for a record taken while the clock was not set. A kernel\n\
            row's principal is `kernel`, its sequence its line's place, and its time, tier\n\
            and level null.\n\
            \n\
            The service log needs the logs grant and the kernel's the kernel-log grant:\n\
            run it as `with admin log`.",
    flags: &[Flag::long_only("kernel", "read the kernel's log instead of the service log")],
};

/// `man`.
pub const MAN: Usage = Usage {
    name: "man",
    about: "usage: man [SECTION] NAME\n\
            \x20      man [--keyword WORD]\n\
            \n\
            Print a page of the manual, the first of that name in sections 1, 8, 2, 5, 7 or\n\
            the one in SECTION. With no NAME, list every page as\n\
            Table<{name: String, section: Int, summary: String}>.",
    flags: &[Flag::new("keyword", 'k', "list the pages whose name or summary holds WORD").with_args("WORD")],
};

/// `terminal`.
pub const TERMINAL: Usage = Usage {
    name: "terminal",
    about: "usage: terminal\n\
            \n\
            Report what is known of the terminal this stage was handed: its size, as its\n\
            window last said, null where it cannot say, as on a serial line. Fails with\n\
            none.\n\
            Emits Table<{columns: Int?, rows: Int?}> on stdout.",
    flags: &[],
};

/// `page`.
pub const PAGE: Usage = Usage {
    name: "page",
    about: "usage: page\n\
            \n\
            Show stdin a page at a time on this stage's terminal, as the shell would show\n\
            it, when that is where it goes. Space, f or Page Down a page on; b or Page Up\n\
            a page back; j, Down or Enter a line on; k or Up a line back; g or Home the\n\
            top; G or End the end; /word the next line holding it, n the one after; q\n\
            quits.\n\
            Input one page holds, or that goes on down a pipeline, passes on unchanged.",
    flags: &[],
};

/// Every coreutil's usage, as `/bin` has them.
pub const PROGRAMS: &[Usage] = &[
    LIST, COPY, MKDIR, REMOVE, RENAME, MOVE, TOUCH, DATE, SLEEP, WHOAMI, DESKTOP, CLIP, WITH, DISK, ACCOUNT, SERVICE,
    SHUTDOWN, LOG, MAN, TERMINAL, PAGE,
];

#[cfg(test)]
mod tests;
