//! **What a device holds**, read the way the server that would serve it reads it: ext4 by
//! `fs-server-ext4`'s own `check_device` (so the service never offers what the server would
//! refuse), FAT by `fs-server-fat`'s own check, or nothing. A boot sector the library cannot
//! parse is still a FAT if [`fat_label`] recognises it, so a FAT with 4 KiB sectors is
//! reported as not served, not as nothing.

use alloc::format;
use alloc::string::String;
use fs_server_ext4::BlockReader;
use fs_server_ext4::ext4::{check_device, volume_label, was_left_clean};
use fs_server_fat::Fat;

/// What a device holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// An ext4 filesystem `fs-server-ext4` can serve.
    Ext4 {
        /// Its own label, empty when it has none.
        label: String,
        /// Whether it was left cleanly unmounted; `None` if the state would not read.
        clean: Option<bool>,
    },
    /// A FAT filesystem.
    Fat {
        /// Its own label, empty when it has none.
        label: String,
        /// Whether it was left cleanly unmounted; `None` if the state would not read.
        clean: Option<bool>,
        /// **Why `fs-server-fat` would refuse it**, in the words its refusal would carry —
        /// `512-byte clusters, smaller than a page` — or `None` if it would serve it.
        refused: Option<String>,
    },
    /// Nothing this service recognises.
    Nothing,
    /// **Ejected**, with the rest of its disk: read no more, as any read wakes a stopped stick.
    Ejected,
}

impl Found {
    /// The filesystem, as the table names it.
    pub fn filesystem(&self) -> Option<&'static str> {
        match self {
            Found::Ext4 { .. } => Some("ext4"),
            Found::Fat { .. } => Some("fat"),
            Found::Nothing | Found::Ejected => None,
        }
    }

    /// The filesystem's own label, if it has one.
    pub fn label(&self) -> Option<&str> {
        match self {
            Found::Ext4 { label, .. } | Found::Fat { label, .. } if !label.is_empty() => Some(label),
            _ => None,
        }
    }

    /// Whether the filesystem was left cleanly unmounted, if it says.
    pub fn clean(&self) -> Option<bool> {
        match self {
            Found::Ext4 { clean, .. } | Found::Fat { clean, .. } => *clean,
            Found::Nothing | Found::Ejected => None,
        }
    }

    /// The server that would serve it; `None` for nothing, or a FAT its server would refuse.
    pub fn server(&self) -> Option<crate::mounts::Server> {
        use crate::mounts::Server;
        match self {
            Found::Ext4 { .. } => Some(Server::Ext4),
            Found::Fat { refused: None, .. } => Some(Server::Fat),
            _ => None,
        }
    }
}

/// What `r` holds.
pub fn probe<R: BlockReader>(r: &R) -> Found {
    if check_device(r).is_ok() {
        let mut raw = [0u8; 16];
        let n = volume_label(r, &mut raw).unwrap_or(0);
        let label = String::from_utf8_lossy(&raw[..n]).into_owned();
        return Found::Ext4 { label, clean: was_left_clean(r).ok() };
    }
    let mut sector = [0u8; 512];
    let recognised = r.read_at(0, &mut sector).ok().and_then(|()| fat_label(&sector));
    let fat = Fat::new(r);
    if fat.geometry().is_err() && recognised.is_none() {
        return Found::Nothing;
    }
    let label = recognised.unwrap_or_else(|| String::from_utf8_lossy(fat.label()).into_owned());
    let refused = fat.check().err().map(|why| format!("{why}"));
    Found::Fat { label, clean: fat.was_left_clean().ok(), refused }
}

/// **What a device holds, by its record**: [`probe`], except that a **whole disk whose first
/// sector carries a partition entry** never holds a filesystem of its own. `sfdisk` and
/// `parted` keep the old bytes, so an ext4's superblock still reads beside the partitions the
/// kernel publishes; mounted whole, it would sit over them. A FAT's boot sector is sector 0
/// itself, which the kernel reads as no table (`kernel/src/drivers/partitions.rs`), so no
/// partition is published and the stale FAT would allocate clusters inside the partition
/// nothing can see: reported, and refused.
pub fn probe_record<R: BlockReader>(r: &R, record: &libkern::device::DeviceRecord) -> Found {
    use libkern::device::DeviceKind;
    let found = probe(r);
    let whole = matches!(record.kind(), DeviceKind::Disk | DeviceKind::RamDisk);
    let mut sector = [0u8; 512];
    if !whole || r.read_at(0, &mut sector).is_err() || !partition_entries(&sector, record.block_count) {
        return found;
    }
    match found {
        Found::Fat { label, clean, .. } => Found::Fat { label, clean, refused: Some(String::from(STALE_FAT)) },
        _ => Found::Nothing,
    }
}

/// Why a whole disk's FAT beside partition entries is not served.
pub const STALE_FAT: &str = "its first sector holds partition entries too, so this FAT may be stale";

/// Whether a disk's first sector carries a partition entry: signed `0x55AA`, every entry's
/// status `0x00` or `0x80` (Linux's check, which boot code rarely passes), and one entry in
/// use that lies on the disk. GPT's protective entry counts.
pub fn partition_entries(sector: &[u8; 512], blocks: u64) -> bool {
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return false;
    }
    let entry = |slot: usize| &sector[0x1BE + 16 * slot..0x1BE + 16 * (slot + 1)];
    if !(0..4).all(|slot| matches!(entry(slot)[0], 0x00 | 0x80)) {
        return false;
    }
    (0..4).any(|slot| {
        let e = entry(slot);
        let first = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
        let count = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
        e[4] != 0 && first != 0 && count != 0 && first.checked_add(count).is_some_and(|end| end <= blocks)
    })
}

/// A FAT boot sector's volume label, or `None` if `sector` is not one: the `0x55 0xAA`
/// signature, a power-of-two sector size from 512 to 4096, the extended boot signature `0x29`,
/// and the type string after it (`FAT32` at `0x52`, or `FAT…` at `0x36`). A GPT disk's
/// protective MBR has the signature and none of the rest. `NO NAME` means no label.
pub fn fat_label(sector: &[u8; 512]) -> Option<String> {
    if sector[510] != 0x55 || sector[511] != 0xAA {
        return None;
    }
    if !matches!(u16::from_le_bytes([sector[11], sector[12]]), 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let label_at = if sector[0x42] == 0x29 && &sector[0x52..0x57] == b"FAT32" {
        0x47
    } else if sector[0x26] == 0x29 && &sector[0x36..0x39] == b"FAT" {
        0x2B
    } else {
        return None;
    };
    let raw = &sector[label_at..label_at + 11];
    let end = raw.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |i| i + 1);
    let label = String::from_utf8_lossy(&raw[..end]).into_owned();
    Some(if label == "NO NAME" { String::new() } else { label })
}
