//! What `nxinstall` decides **before** it writes anything: where the two partitions go, and
//! every reason to refuse.
//!
//! Split out from the program so it can be host-tested, for the reason the whole part exists:
//! the mistakes that matter here are arithmetic, they destroy a disk, and none of them is
//! visible in a boot that succeeds. A root partition that runs one block into the backup
//! partition array produces a system that boots perfectly and loses its table the first time
//! something rewrites it (`libgpt`'s own `last_usable` test, PR #308 review).
//!
//! **Numbers, not handles.** This module takes what a device *reported* and returns extents; it
//! opens nothing, reads nothing and cannot write. That is what lets it depend on `libgpt` alone
//! and run on the host.

#![cfg_attr(not(test), no_std)]

use libgpt::table::{ARRAY_BLOCKS, FIRST_USABLE, Partition, TYPE_EFI_SYSTEM, TYPE_LINUX_FS};

/// Partition alignment, in 512-byte blocks: 1 MiB, which is what every partitioner has used
/// since 4 Kn drives arrived. Aligning to the *logical* block size is not enough — a 4 KiB
/// physical sector under 512-byte logical addressing turns a misaligned write into a
/// read-modify-write of the neighbouring sector.
pub const ALIGN: u64 = 2048;

/// Where the ESP starts: 1 MiB in, leaving room for the table and the alignment gap.
pub const ESP_FIRST: u64 = ALIGN;

/// The GPT name of the boot partition. Nothing resolves this — the firmware finds an ESP by its
/// *type* GUID — so it exists to be recognised by a person looking at the disk later.
pub const ESP_LABEL: &[u8] = b"NITROX_ESP";

/// The GPT name of the root partition, which the **release `init.toml` names** as
/// `gpt-partlabel:nitrox-root`. An installed system finds its root by this string, so it is the
/// one label here that is load-bearing.
pub const ROOT_LABEL: &[u8] = b"nitrox-root";

/// The only logical block size this installer writes. `libgpt` addresses a disk in 512-byte
/// blocks throughout, so a 4 Kn disk would need every LBA in every structure reinterpreted —
/// refused rather than written wrongly.
pub const LOGICAL_BLOCK: u32 = 512;

/// What a run of the installer did, and therefore what it exits with.
///
/// **The status answers "did what you asked for happen", not "was anything written".** Asking
/// what an install *would* do — a device with no identity after it — and being told is a
/// success: it is a query, it answered, and it says in its own words that nothing was written.
/// Treating it as a failure made the shell print `pipeline failed` directly underneath a calm
/// explanation of what to type next, which reads as though something had broken (first install
/// attempt on the laptop, 2026-09-17).
///
/// Asking for an install and not getting one is a failure whatever refused it — a wrong kind of
/// device, a name that did not match, a disk too small, an I/O error part-way. In all of those
/// the caller asked for something that did not happen, which is exactly what a non-zero status
/// is for, and a script that stops on it stops correctly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The devices this session can reach were listed.
    Listed,
    /// What an install would do was reported. Nothing was written.
    Planned,
    /// The install completed.
    Installed,
    /// An install was asked for and did not happen. Nothing was written, or the failure says
    /// how far it got.
    NotInstalled,
    /// The command line was not one this program accepts.
    Usage,
}

impl Outcome {
    /// The process exit status.
    pub fn status(self) -> i64 {
        match self {
            Outcome::Listed | Outcome::Planned | Outcome::Installed => 0,
            Outcome::NotInstalled => 1,
            Outcome::Usage => 2,
        }
    }
}

/// Why a target cannot be installed to. Each is a sentence a person can act on, which is why
/// the numbers travel with the variant rather than being formatted away here.
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

    /// The two entries, for [`libgpt::table::build`].
    ///
    /// The GUIDs are the caller's, because this crate has no entropy and `libgpt` deliberately
    /// invents none: a disk whose partitions all share one hard-coded GUID is indistinguishable
    /// from every other machine's, and something will eventually care.
    pub fn partitions(&self, esp_guid: [u8; 16], root_guid: [u8; 16]) -> [Partition; 2] {
        [
            Partition::new(TYPE_EFI_SYSTEM, esp_guid, self.esp_first, self.esp_last, ESP_LABEL),
            Partition::new(TYPE_LINUX_FS, root_guid, self.root_first, self.root_last, ROOT_LABEL),
        ]
    }
}

/// Round `blocks` up to the next [`ALIGN`] boundary.
fn align_up(blocks: u64) -> u64 {
    blocks.div_ceil(ALIGN) * ALIGN
}

/// Place both partitions on a disk of `disk_blocks` blocks, given the sizes of the two sources.
///
/// `esp_blocks` is the whole installable-ESP image, which is copied in as raw sectors; the boot
/// partition is exactly that image, since a FAT32 filesystem records its own size and growing
/// the partition around it would only produce a partition whose tail no filesystem knows about.
///
/// `root_min_blocks` is the filesystem being copied into the root partition. The partition takes
/// **the rest of the disk** rather than that size: the filesystem inside it stays as big as it
/// was — growing it is Part H.2's job — and a partition sized to today's filesystem would have
/// to be moved before it could ever be grown.
pub fn plan(
    disk_blocks: u64,
    block_size: u32,
    esp_blocks: u64,
    root_min_blocks: u64,
) -> Result<Layout, PlanError> {
    if block_size != LOGICAL_BLOCK {
        return Err(PlanError::BlockSize(block_size));
    }
    let esp_first = ESP_FIRST;
    let esp_last = esp_first + esp_blocks.max(1) - 1;
    let root_first = align_up(esp_last + 1);
    // **The last block the table may use**, which is where `libgpt::build` puts its own ceiling:
    // the backup header takes the final block and the backup array the `ARRAY_BLOCKS` before it.
    // Computed here as well as there so this function can say *why* a disk is too small, with a
    // number, rather than handing back `libgpt`'s `DiskTooSmall` after the person has already
    // typed the disk's name back.
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

#[cfg(test)]
mod tests {
    use super::*;
    use libgpt::table::{self, BACK_BYTES, FRONT_BYTES};

    /// A 1 GiB disk, a 33 MiB ESP and a 24 MiB root — the shape `check-install` boots.
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
        // The rest of the disk, not the size of the filesystem going into it.
        assert!(l.root_blocks() > 900 * 2048, "a 1 GiB disk should yield most of itself");
    }

    /// **An ESP that is not a whole number of MiB**, which is the only input that makes the
    /// assertion above mean anything. Every other test here passes a 33 MiB source, and
    /// `ESP_FIRST + 33 * 2048` is already aligned — so `align_up` was the identity on the whole
    /// suite, and deleting it left all seven tests passing (PR #309 review, blocking 1).
    ///
    /// The real input is `byte_capacity() / 512` of whatever RAM disk the module became.
    /// `assemble_live_image` happens to size that to a whole MiB today, so `check-install`
    /// cannot see this either; size it to its contents instead, or copy from anywhere else, and
    /// nothing but this test stands between a misaligned partition and every write to it
    /// costing a read-modify-write of the neighbouring physical sector.
    #[test]
    fn an_esp_that_is_not_a_whole_number_of_mib_still_aligns_the_root() {
        let odd = plan(2 * 1024 * 1024, 512, 33 * 2048 + 1, 24 * 2048).expect("still fits");
        assert_eq!(odd.esp_last, ESP_FIRST + 33 * 2048, "the ESP is its own size, unrounded");
        assert_eq!(odd.root_first % ALIGN, 0, "and the root still starts on a boundary");
        assert_eq!(odd.root_first, 35 * 2048, "the next boundary after the ESP, not the block after it");
        // The control, stated here rather than inferred: without rounding, the root would start
        // on the block straight after the ESP, which this asserts is *not* where it is.
        assert_ne!(odd.root_first, odd.esp_last + 1);
    }

    /// The failure `libgpt`'s own `last_usable` test exists for, from the other side: a root
    /// partition that runs into the backup array boots fine and loses the table later.
    #[test]
    fn the_root_partition_stops_short_of_the_backup_table() {
        let disk = 2 * 1024 * 1024;
        let l = plan(disk, 512, 33 * 2048, 24 * 2048).unwrap();
        assert_eq!(l.root_last, disk - ARRAY_BLOCKS - 2);
        // And the proof that the number agrees with the writer: `build` accepts it, and one
        // block more is refused.
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

    /// The table this plan produces reads back as the two partitions it named — the round trip
    /// that an installed machine's firmware and `init` each do half of.
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
        // Room for the ESP and the tables, none for the root.
        let err = plan(34 * 2048, 512, 33 * 2048, 24 * 2048).unwrap_err();
        match err {
            PlanError::TooSmall { have, need } => {
                assert_eq!(have, 34 * 2048);
                assert!(need > have, "the message has to name a bigger number to be useful");
            }
            other => panic!("expected TooSmall, got {other:?}"),
        }
        // A disk one block short of enough is still short — the boundary, not just the shape.
        let l = small_disk();
        let exact = l.root_first + 24 * 2048 + ARRAY_BLOCKS + 1;
        assert!(plan(exact, 512, 33 * 2048, 24 * 2048).is_ok(), "control: exactly enough");
        assert!(plan(exact - 1, 512, 33 * 2048, 24 * 2048).is_err(), "one block short");
    }

    /// **A question answered is not a failure.** The two-step confirmation is the *ordinary*
    /// path through this program — a person is meant to run the one-operand form, read it, and
    /// then run what it prints — so reporting it as a failure puts `pipeline failed` under
    /// every correct use of the installer.
    #[test]
    fn asking_what_would_happen_succeeds_and_asking_for_an_install_that_did_not_happen_does_not() {
        assert_eq!(Outcome::Planned.status(), 0, "a plan is an answer, not a failure");
        assert_eq!(Outcome::Listed.status(), 0);
        assert_eq!(Outcome::Installed.status(), 0);
        // And the half that must stay non-zero: a script that asked for an install and did not
        // get one has to be able to stop.
        assert_ne!(Outcome::NotInstalled.status(), 0);
        assert_ne!(Outcome::Usage.status(), 0);
        // Usage is distinguishable from a refusal, which is the split every program here uses.
        assert_ne!(Outcome::Usage.status(), Outcome::NotInstalled.status());
    }

    /// 4 Kn disks exist, `libgpt` addresses 512-byte blocks throughout, and the difference is
    /// invisible until the firmware reads a table at the wrong offset.
    #[test]
    fn a_disk_that_does_not_address_512_byte_blocks_is_refused() {
        assert_eq!(
            plan(2 * 1024 * 1024, 4096, 33 * 2048, 24 * 2048),
            Err(PlanError::BlockSize(4096))
        );
    }
}
