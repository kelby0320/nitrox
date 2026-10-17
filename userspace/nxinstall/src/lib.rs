//! What `nxinstall` decides before it writes anything: where the two partitions go, and every
//! reason to refuse.
//!
//! Split out so it can be host-tested: the mistakes that matter here are arithmetic, they destroy a
//! disk, and none is visible in a boot that succeeds. The planning code opens nothing and cannot
//! write; [`copy`] and [`account`] write through `fs_server_ext4`'s `BlockWriter`, so a host test
//! hands them a disk in memory.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod account;
pub mod copy;

use libgpt::table::{ARRAY_BLOCKS, FIRST_USABLE, Partition, TYPE_EFI_SYSTEM, TYPE_LINUX_FS};

/// Partition alignment, in 512-byte blocks: 1 MiB. Aligning to the logical block size is not
/// enough: a 4 KiB physical sector under 512-byte logical addressing turns a misaligned write into
/// a read-modify-write of the neighbouring sector.
pub const ALIGN: u64 = 2048;

/// Where the ESP starts: 1 MiB in, leaving room for the table and the alignment gap.
pub const ESP_FIRST: u64 = ALIGN;

/// The GPT name of the boot partition. Nothing resolves it (the firmware finds an ESP by its
/// type GUID); it is for a person looking at the disk.
pub const ESP_LABEL: &[u8] = b"NITROX_ESP";

/// The GPT name of the root partition. The release `init.toml` names it as
/// `gpt-partlabel:nitrox-root`, so an installed system finds its root by this string.
pub const ROOT_LABEL: &[u8] = b"nitrox-root";

/// The only logical block size this installer writes: `libgpt` addresses a disk in 512-byte
/// blocks throughout, so a 4 Kn disk is refused rather than written wrongly.
pub const LOGICAL_BLOCK: u32 = 512;

/// The index of a child of `/dev/blk` named `name`, if it names a device: decimal, with no
/// leading zero but `0` itself, so one device has one spelling.
pub fn block_index(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) || (bytes.len() > 1 && bytes[0] == b'0') {
        return None;
    }
    name.parse().ok()
}

/// What a run of the installer did, and therefore what it exits with.
///
/// The status answers "did what you asked for happen", not "was anything written": asking what an
/// install would do succeeds, and so does saying no to the plan or cancelling a question. Asking
/// for an install and not getting one is a failure whatever refused it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The devices this view can reach were listed.
    Listed,
    /// The usage was printed, as `--help` asks.
    Help,
    /// The person was asked to go ahead, or for the account, and did not. Nothing was written.
    Declined,
    /// The install completed.
    Installed,
    /// An install was asked for and did not happen; the failure says how far it got.
    NotInstalled,
    /// The command line was not one this program accepts.
    Usage,
}

impl Outcome {
    /// The process exit status.
    pub fn status(self) -> i64 {
        match self {
            Outcome::Listed | Outcome::Help | Outcome::Declined | Outcome::Installed => 0,
            Outcome::NotInstalled => 1,
            Outcome::Usage => 2,
        }
    }
}

/// Whether `answer` is the go-ahead: `yes`, in any case, surrounding blanks ignored, and nothing
/// else. Not `y`, which is a reflex, nor a longer word like `yesterday`.
pub fn confirmed(answer: &[u8]) -> bool {
    answer.trim_ascii().eq_ignore_ascii_case(b"yes")
}

/// Why a target cannot be installed to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// The device addresses blocks of some size other than 512.
    BlockSize(u32),
    /// The disk is smaller than the two partitions plus the tables at both ends.
    TooSmall {
        /// Blocks the disk has.
        have: u64,
        /// Blocks it would need.
        need: u64,
    },
}

/// Where the two partitions land on the target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// First block of the boot partition.
    pub esp_first: u64,
    /// Last block of the boot partition, inclusive.
    pub esp_last: u64,
    /// First block of the root partition.
    pub root_first: u64,
    /// Last block of the root partition, inclusive — the last block the table may use.
    pub root_last: u64,
}

impl Layout {
    /// Blocks in the boot partition.
    pub fn esp_blocks(&self) -> u64 {
        self.esp_last - self.esp_first + 1
    }

    /// Blocks in the root partition.
    pub fn root_blocks(&self) -> u64 {
        self.root_last - self.root_first + 1
    }

    /// The two entries, for [`libgpt::table::build`]. The GUIDs are the caller's: this crate has no
    /// entropy, and a hard-coded GUID would be shared with every other machine's disk.
    pub fn partitions(&self, esp_guid: [u8; 16], root_guid: [u8; 16]) -> [Partition; 2] {
        [
            Partition::new(TYPE_EFI_SYSTEM, esp_guid, self.esp_first, self.esp_last, ESP_LABEL),
            Partition::new(TYPE_LINUX_FS, root_guid, self.root_first, self.root_last, ROOT_LABEL),
        ]
    }
}

fn align_up(blocks: u64) -> u64 {
    blocks.div_ceil(ALIGN) * ALIGN
}

/// Place both partitions on a disk of `disk_blocks` blocks, given the sizes of the two sources.
///
/// The boot partition is exactly `esp_blocks`, the ESP image copied in as raw sectors: a FAT32
/// filesystem records its own size, so a larger partition would have a tail it does not know.
/// The root partition takes the rest of the disk, not `root_min_blocks`, so the filesystem can
/// later be grown without moving the partition.
pub fn plan(disk_blocks: u64, block_size: u32, esp_blocks: u64, root_min_blocks: u64) -> Result<Layout, PlanError> {
    if block_size != LOGICAL_BLOCK {
        return Err(PlanError::BlockSize(block_size));
    }
    let esp_first = ESP_FIRST;
    let esp_last = esp_first + esp_blocks.max(1) - 1;
    let root_first = align_up(esp_last + 1);
    // The last usable block is where `libgpt::build` puts its ceiling: the backup header takes
    // the final block and the backup array the `ARRAY_BLOCKS` before it. Computed here too so a
    // too-small disk is refused with a number before the person types the disk's name back.
    let need = root_first + root_min_blocks + ARRAY_BLOCKS + 1;
    if disk_blocks < need || disk_blocks < FIRST_USABLE {
        return Err(PlanError::TooSmall { have: disk_blocks, need });
    }
    let root_last = disk_blocks - ARRAY_BLOCKS - 2;
    if root_last < root_first {
        return Err(PlanError::TooSmall { have: disk_blocks, need });
    }
    Ok(Layout { esp_first, esp_last, root_first, root_last })
}

pub mod withheld {
    //! A disk the view does not hold, and why.
    //!
    //! The view's `disks` grant is every block device but those in use, so a disk the machine has
    //! and the view lacks is in use: mounted by the storage service (`disk --unmount` takes the
    //! mount's label, the last part of `/storage/<label>`, not the path), or by `init`, which is
    //! the running system and is never unmounted.

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// A block device as `/dev/devices` lists it. An empty field is one the table left `Null`.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Device {
        /// `blk-<n>`, which is also its name in `/dev/storage`.
        pub name: String,
        /// `disk`, `partition` or `ramdisk`.
        pub kind: String,
        /// `/dev/blk/<n>`.
        pub path: String,
        /// A disk's model and serial, a RAM disk's module.
        pub description: String,
        /// The name of the device it belongs to: a partition's disk.
        pub parent: String,
        /// The disk the machine started from: on a live boot, the stick.
        pub boot: bool,
    }

    /// A block device as the storage service reports it. An empty field is `Null`.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Mount {
        /// `blk-<n>`.
        pub name: String,
        /// Where a filesystem on it is mounted: `/`, or `/storage/<label>`.
        pub at: String,
        /// Who mounted it: `init` or `storage`.
        pub by: String,
    }

    /// Why a disk is not in the view.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Why {
        /// The storage service mounted a filesystem on it: the mount's label, and where.
        Mounted {
            /// What `disk --unmount` takes.
            label: String,
            /// `/storage/<label>`.
            at: String,
        },
        /// `init` mounted a filesystem on it: it holds the running system.
        Running,
    }

    /// A disk the view does not hold.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Withheld {
        /// `/dev/blk/<n>`, as it would be in the view.
        pub path: String,
        /// Its model and serial, or its module.
        pub description: String,
        /// Why.
        pub why: Why,
    }

    /// Every disk and RAM disk the view does not reach that something has mounted or the machine
    /// started from, in the table's order. `reachable` is the view's own `/dev/blk/<n>` paths; a
    /// disk missing for no reason the tables give is left out.
    pub fn withheld(devices: &[Device], mounts: &[Mount], reachable: &[&str]) -> Vec<Withheld> {
        let mut out = Vec::new();
        for d in devices.iter().filter(|d| (d.kind == "disk" || d.kind == "ramdisk") && !d.path.is_empty()) {
            if reachable.contains(&d.path.as_str()) {
                continue;
            }
            let on = |name: &str| mounts.iter().find(|m| m.name == name && !m.at.is_empty());
            let mut found: Vec<&Mount> = on(&d.name).into_iter().collect();
            for p in devices.iter().filter(|p| p.parent == d.name) {
                found.extend(on(&p.name));
            }
            // The boot disk holds the running system whether or not anything on it is mounted
            // (the live stick, its system in RAM).
            let why = if d.boot || found.iter().any(|m| m.by == "init") {
                Why::Running
            } else if let Some(m) = found.iter().find(|m| m.by == "storage") {
                let label = String::from(m.at.rsplit('/').next().unwrap_or_default());
                Why::Mounted { label, at: m.at.clone() }
            } else {
                continue;
            };
            out.push(Withheld { path: d.path.clone(), description: d.description.clone(), why });
        }
        out
    }

    impl Withheld {
        /// What to tell the person, in one line.
        pub fn message(&self) -> String {
            match &self.why {
                Why::Mounted { label, at } => format!(
                    "{} ({}) is in use: {label} is mounted at {at}. Unmount it with `with admin disk \
                     --unmount {label}`, then run nxinstall again.",
                    self.path, self.description
                ),
                Why::Running => format!("{} ({}) holds the running system.", self.path, self.description),
            }
        }

        /// What the system log says when a person names it as the target.
        pub fn refusal(&self) -> &'static str {
            match self.why {
                Why::Mounted { .. } => "it is in use",
                Why::Running => "it holds the running system",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libgpt::table::{self, BACK_BYTES, FRONT_BYTES};

    #[test]
    fn a_device_is_a_child_named_by_its_index() {
        assert_eq!(block_index("0"), Some(0));
        assert_eq!(block_index("2"), Some(2));
        assert_eq!(block_index("15"), Some(15));
        for not in ["", "info", "01", "1a", "-1", " 1"] {
            assert_eq!(block_index(not), None, "{not:?}");
        }
    }

    /// The shape `check-install` boots.
    fn small_disk() -> Layout {
        plan(2 * 1024 * 1024, 512, 33 * 2048, 24 * 2048).expect("a 1 GiB disk is enough")
    }

    #[test]
    fn the_boot_partition_is_the_esp_image_and_starts_at_one_mib() {
        let l = small_disk();
        assert_eq!(l.esp_first, 2048, "1 MiB in");
        assert_eq!(l.esp_blocks(), 33 * 2048, "exactly the image being copied");
    }

    #[test]
    fn the_root_partition_is_aligned_and_takes_the_rest() {
        let l = small_disk();
        assert_eq!(l.root_first % ALIGN, 0, "a misaligned partition costs every write twice");
        assert!(l.root_first > l.esp_last, "the two must not overlap");
        assert!(l.root_blocks() > 900 * 2048, "a 1 GiB disk should yield most of itself");
    }

    /// Only an ESP that is not a whole number of MiB pins `align_up`.
    #[test]
    fn an_esp_that_is_not_a_whole_number_of_mib_still_aligns_the_root() {
        let odd = plan(2 * 1024 * 1024, 512, 33 * 2048 + 1, 24 * 2048).expect("still fits");
        assert_eq!(odd.esp_last, ESP_FIRST + 33 * 2048, "the ESP is its own size, unrounded");
        assert_eq!(odd.root_first % ALIGN, 0, "and the root still starts on a boundary");
        assert_eq!(odd.root_first, 35 * 2048, "the next boundary after the ESP, not the block after it");
        assert_ne!(odd.root_first, odd.esp_last + 1);
    }

    #[test]
    fn the_root_partition_stops_short_of_the_backup_table() {
        let disk = 2 * 1024 * 1024;
        let l = plan(disk, 512, 33 * 2048, 24 * 2048).unwrap();
        assert_eq!(l.root_last, disk - ARRAY_BLOCKS - 2);
        // `build` accepts it, and one block more is refused.
        let (mut front, mut back) = ([0u8; FRONT_BYTES], [0u8; BACK_BYTES]);
        let ps = l.partitions([1; 16], [2; 16]);
        assert!(table::build(disk, [3; 16], &ps, &mut front, &mut back).is_ok());
        let mut over = ps;
        over[1] = Partition::new(TYPE_LINUX_FS, [2; 16], l.root_first, l.root_last + 1, ROOT_LABEL);
        assert_eq!(
            table::build(disk, [3; 16], &over, &mut front, &mut back),
            Err(table::BuildError::OutOfRange),
            "one block further is over the line, and the writer agrees"
        );
    }

    #[test]
    fn the_table_reads_back_as_the_plan() {
        let disk = 2 * 1024 * 1024;
        let l = plan(disk, 512, 33 * 2048, 24 * 2048).unwrap();
        let (mut front, mut back) = ([0u8; FRONT_BYTES], [0u8; BACK_BYTES]);
        table::build(disk, [3; 16], &l.partitions([1; 16], [2; 16]), &mut front, &mut back).unwrap();
        let t = table::read(&front).expect("what we just wrote is readable");
        let root = t.by_name(ROOT_LABEL).expect("an installed system finds its root by this name");
        assert_eq!((root.first_lba, root.last_lba), (l.root_first, l.root_last));
        let esp = t.by_name(ESP_LABEL).expect("and a person finds the boot partition by this one");
        assert_eq!((esp.first_lba, esp.last_lba), (l.esp_first, l.esp_last));
        assert_eq!(esp.type_guid, TYPE_EFI_SYSTEM, "firmware finds it by type, not by name");
    }

    #[test]
    fn a_disk_too_small_for_both_is_refused_with_the_number_it_needed() {
        let err = plan(34 * 2048, 512, 33 * 2048, 24 * 2048).unwrap_err();
        match err {
            PlanError::TooSmall { have, need } => {
                assert_eq!(have, 34 * 2048);
                assert!(need > have, "the message has to name a bigger number to be useful");
            }
            other => panic!("expected TooSmall, got {other:?}"),
        }
        let l = small_disk();
        let exact = l.root_first + 24 * 2048 + ARRAY_BLOCKS + 1;
        assert!(plan(exact, 512, 33 * 2048, 24 * 2048).is_ok(), "control: exactly enough");
        assert!(plan(exact - 1, 512, 33 * 2048, 24 * 2048).is_err(), "one block short");
    }

    #[test]
    fn saying_no_succeeds_and_asking_for_an_install_that_did_not_happen_does_not() {
        assert_eq!(Outcome::Declined.status(), 0, "saying no is an answer, not a failure");
        assert_eq!(Outcome::Help.status(), 0);
        assert_eq!(Outcome::Listed.status(), 0);
        assert_eq!(Outcome::Installed.status(), 0);
        assert_ne!(Outcome::NotInstalled.status(), 0);
        assert_ne!(Outcome::Usage.status(), 0);
        assert_ne!(Outcome::Usage.status(), Outcome::NotInstalled.status());
    }

    #[test]
    fn only_yes_goes_ahead() {
        for yes in [&b"yes"[..], b"YES", b"Yes", b"  yes ", b"yes\r", b"\tyes"] {
            assert!(confirmed(yes), "{:?} is the go-ahead", core::str::from_utf8(yes));
        }
        for no in [&b""[..], b"y", b"Y", b"no", b"ye", b"yess", b"yesterday", b"yes please", b"oui", b"\n"] {
            assert!(!confirmed(no), "{:?} is not", core::str::from_utf8(no));
        }
    }

    #[test]
    fn a_disk_that_does_not_address_512_byte_blocks_is_refused() {
        assert_eq!(plan(2 * 1024 * 1024, 4096, 33 * 2048, 24 * 2048), Err(PlanError::BlockSize(4096)));
    }

    use super::withheld::{Device, Mount, Why, Withheld, withheld};

    fn dev(name: &str, kind: &str, path: &str, description: &str, parent: &str) -> Device {
        Device {
            name: name.into(),
            kind: kind.into(),
            path: path.into(),
            description: description.into(),
            parent: parent.into(),
            boot: false,
        }
    }

    fn mount(name: &str, at: &str, by: &str) -> Mount {
        Mount { name: name.into(), at: at.into(), by: by.into() }
    }

    #[test]
    fn a_withheld_disk_is_named_with_what_frees_it() {
        let devices = [
            dev("blk-0", "disk", "/dev/blk/0", "QEMU HARDDISK (QM00002)", "pci-0"),
            dev("blk-1", "ramdisk", "/dev/blk/1", "module 1 (/boot/root.img)", ""),
            dev("blk-2", "ramdisk", "/dev/blk/2", "module 2 (/boot/install-esp.img)", ""),
            dev("blk-3", "ramdisk", "/dev/blk/3", "module 3 (/boot/install-root.img)", ""),
            dev("blk-4", "partition", "/dev/blk/4", "NITROX_ESP", "blk-0"),
            dev("blk-5", "partition", "/dev/blk/5", "nitrox-root", "blk-0"),
            dev("blk-6", "partition", "/dev/blk/6", "nitrox-live", "blk-1"),
            dev("blk-7", "partition", "/dev/blk/7", "nitrox-source", "blk-3"),
            dev("kbd-0", "keyboard", "", "", ""),
        ];
        let mounts =
            [mount("blk-5", "/storage/nitrox-root", "storage"), mount("blk-6", "/", "init"), mount("blk-7", "", "")];
        let reachable = ["/dev/blk/2", "/dev/blk/3", "/dev/blk/7"];
        let w = withheld(&devices, &mounts, &reachable);
        assert_eq!(
            w,
            [
                Withheld {
                    path: "/dev/blk/0".into(),
                    description: "QEMU HARDDISK (QM00002)".into(),
                    why: Why::Mounted { label: "nitrox-root".into(), at: "/storage/nitrox-root".into() },
                },
                Withheld {
                    path: "/dev/blk/1".into(),
                    description: "module 1 (/boot/root.img)".into(),
                    why: Why::Running,
                },
            ]
        );
        assert!(w[0].message().contains("`with admin disk --unmount nitrox-root`"), "{}", w[0].message());
        assert!(!w[0].message().contains("--unmount /storage"));
        assert!(w[1].message().ends_with("holds the running system."));
        assert!(!w[1].message().contains("unmount"), "init's mounts are never unmounted");
        assert_eq!((w[0].refusal(), w[1].refusal()), ("it is in use", "it holds the running system"));
    }

    #[test]
    fn the_disk_the_machine_started_from_holds_the_running_system() {
        let mut stick = dev("blk-4", "disk", "/dev/blk/4", "QEMU QEMU HARDDISK (1-0000:00:04.0-1)", "usb-12");
        stick.boot = true;
        let w = withheld(&[stick.clone()], &[], &[]);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].why, Why::Running);
        assert!(w[0].message().ends_with("holds the running system."), "{}", w[0].message());
        stick.boot = false;
        assert!(withheld(&[stick], &[], &[]).is_empty(), "nothing gives a reason, so nothing is said");
    }

    #[test]
    fn only_a_mounted_disk_the_view_lacks_is_named() {
        let devices = [
            dev("blk-0", "disk", "/dev/blk/0", "QEMU HARDDISK (QM00001)", "pci-0"),
            dev("blk-1", "partition", "/dev/blk/1", "NITROX_ESP", "blk-0"),
            dev("blk-2", "partition", "/dev/blk/2", "nitrox-root", "blk-0"),
            dev("blk-3", "disk", "/dev/blk/3", "SPARE (S1)", "pci-0"),
            dev("blk-4", "disk", "/dev/blk/4", "HELD (H1)", "pci-0"),
        ];
        let mounts = [mount("blk-2", "/", "init"), mount("blk-4", "/storage/data", "storage")];
        let w = withheld(&devices, &mounts, &["/dev/blk/1", "/dev/blk/4"]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!((w[0].path.as_str(), &w[0].why), ("/dev/blk/0", &Why::Running));
        let bare = withheld(&devices, &mounts, &["/dev/blk/1"]);
        assert_eq!(bare[1].why, Why::Mounted { label: "data".into(), at: "/storage/data".into() });
    }
}
