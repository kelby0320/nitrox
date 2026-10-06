//! **A disk's partitions, published** — GPT's, and since Phase 6 Part D.1 MBR's.
//!
//! [`partitions`](super::partitions) parses the table; this publishes each partition as a block
//! [`DeviceNode`] over a [`Partition`](crate::io::block::Partition) window — the first
//! **two-layer** block IRP stack (partition rebases the offset and forwards to the disk). The
//! partition nodes are registered in the device table (so they appear at `/dev/blk/<n>`), and **a
//! boot disk's GPT partitions** are recorded for the stable `/dev/disk/by-partuuid/<uuid>` and
//! `/dev/disk/by-partlabel/<label>` namespace bindings (created at boot by
//! [`bind_partition_names`]). **A USB disk's are not** ([`Names`]): those names are how `init`
//! finds its critical path, among what the boot's probe found.
//!
//! **The disk the machine started from is flagged** in its record (`BOOT`), when its GPT's GUID is
//! the one Limine loaded the modules from ([`partitions::is_boot`]).
//!
//! At boot [`init`] reads with interrupts masked, so it uses the synchronous polled
//! [`read_blocking`](crate::io::block::read_blocking). GPT header/array CRC validation is deferred
//! (the signature + sane bounds are checked).

use super::partitions::{self, PartKind, Scheme, Table, Unread};
use crate::io::block::{Partition, partition_backend, read_blocking};
use crate::libkern::block::{BlockKind, NameBuf, MAX_DEVICE_NAME};
use crate::libkern::handle::KObjectType;
use crate::libkern::{KBox, KVec, Rights, SpinLock};
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::{Namespace, ObjectRef};
use crate::libkern::lockrank::LockRank;

const SECTOR: u64 = partitions::BLOCK as u64;

/// One published partition, retained for the deferred namespace bindings.
struct PartEntry {
    node: ObjectRef,
    /// `/dev/disk/by-partuuid/<uuid>` path.
    by_partuuid: KVec<u8>,
    /// `/dev/disk/by-partlabel/<label>` path (absent if the label is empty or
    /// not a usable path component).
    by_partlabel: Option<KVec<u8>>,
}

/// Partitions discovered across all disks, for [`bind_partition_names`]. Written
/// at boot by [`init`]; read once when init's namespace is built.
static PARTITIONS: SpinLock<KVec<PartEntry>> = SpinLock::new(LockRank::Registry, KVec::new());

/// Whether a disk's partitions are recorded for `/dev/disk`.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Names {
    /// A disk the boot's probe found: its GPT partitions are recorded for
    /// [`bind_partition_names`].
    Bind,
    /// A disk that arrived later, a USB one (Phase 6 Part D): none are.
    None,
}

/// **Read `disk`'s table at boot and publish its partitions**, with their `/dev/disk` names.
/// Interrupts masked, so the reads are polled.
pub fn init(disk: &ObjectRef) {
    // SAFETY: `disk` pins a live `DeviceNode`.
    let blocks = unsafe { &*(disk.as_ptr() as *const DeviceNode) }.geometry().block_count;
    let table = partitions::read(blocks, &mut |lba, count, out: &mut [u8]| read_blocking(disk, lba, count, out));
    publish(disk, table, Names::Bind);
}

/// **Publish what `table` says of `disk`**, logging each partition, and flag the disk if it is the
/// one the machine started from. How many partitions were published.
pub fn publish(disk: &ObjectRef, table: Result<Table, Unread>, names: Names) -> u32 {
    let table = match table {
        Ok(t) => t,
        Err(Unread::Io) => {
            crate::kprintln!("partitions: the table did not read");
            return 0;
        }
        Err(Unread::Unsupported(why)) => {
            crate::kprintln!("gpt: {why}; not read");
            return 0;
        }
        Err(Unread::NoMemory) => {
            crate::kprintln!("partitions: no memory for the table");
            return 0;
        }
    };
    let what = match table.scheme {
        Scheme::Gpt { disk_guid } => {
            if partitions::is_boot(&disk_guid, partitions::boot_disk()) && crate::device::mark_boot(disk) {
                crate::kprintln!("gpt: the disk the machine started from: its GUID is the one Limine loaded from");
            }
            "gpt"
        }
        Scheme::Mbr => "mbr",
        Scheme::None => {
            crate::kprintln!("partitions: no table: neither a GPT nor an MBR");
            return 0;
        }
    };
    let mut found = 0u32;
    for p in table.parts.iter() {
        if publish_partition(disk, p, names) {
            found += 1;
        }
    }
    crate::kprintln!("{what}: {} partition(s)", found);
    if table.extended > 0 {
        crate::kprintln!("mbr: {} extended partition(s) passed over: their logical partitions are not read", table.extended);
    }
    found
}

/// Create a partition window + block `DeviceNode`, register it, and — for a boot disk's GPT
/// partition — record its `by-partuuid`/`by-partlabel` paths.
fn publish_partition(disk: &ObjectRef, p: &partitions::Part, names: Names) -> bool {
    let Some(part) = Partition::new(disk, p.first_lba, p.count, SECTOR) else {
        return false;
    };
    let backend = partition_backend(part);
    // SAFETY: `disk` pins a live `DeviceNode`; copy its bus identity.
    let dd = unsafe { &*(disk.as_ptr() as *const DeviceNode) }.descriptor();
    let descriptor = ResourceDescriptor {
        identity: DeviceIdentity {
            vendor: dd.identity.vendor,
            device: dd.identity.device,
            class: 0x01,
            subclass: 0x06,
            prog_if: 0x01,
            revision: dd.identity.revision,
        },
        bars: [BarWindow::ZERO; 6],
        interrupt: InterruptSpec::NONE,
        seg: dd.seg,
        bus: dd.bus,
        dev: dd.dev,
        func: dd.func,
        _pad: [0; 3],
    };
    let geometry = BlockGeometry {
        logical_block_size: SECTOR as u32,
        block_count: p.count,
    };
    let (gpt, mbr_kind) = match &p.kind {
        PartKind::Gpt { guid, name } => (Some((guid, name)), 0),
        PartKind::Mbr { kind } => (None, *kind),
    };
    let by_partlabel = gpt.and_then(|(_, name)| decode_partlabel(&name[..]));
    // **A partition's name is its label** — what `init.toml` selects it by, and what a person
    // reading a disk list recognises. An unlabelled one says which slice of which disk it is,
    // because "partition" alone is not something you can confirm before destroying it.
    let mut name = [0u8; MAX_DEVICE_NAME];
    let name_len = match by_partlabel.as_ref() {
        // `decode_partlabel` returns the whole `/dev/disk/by-partlabel/<label>` path, because that
        // is what it is for. A device's name is the label itself: a person confirming a partition
        // reads `nitrox-root`, not a path they cannot type anywhere.
        Some(path) => {
            let label = path.strip_prefix(PARTLABEL_PREFIX).unwrap_or(&path[..]);
            let n = label.len().min(MAX_DEVICE_NAME);
            name[..n].copy_from_slice(&label[..n]);
            n
        }
        None => {
            let mut w = NameBuf::new(&mut name);
            // **Numbered from one, as every other tool numbers partitions.** A position in an
            // array counts from zero; a partition *number* is
            // 1-based everywhere a person will have met one — `/dev/sda1`, `sgdisk`'s
            // listing, the firmware's `HD(1,GPT,…)`. The disk list on the laptop showed
            // `partition 0`, `partition 1`, `partition 2` against a Debian install whose
            // own tools call the same three 1, 2 and 3 (2026-09-17).
            let _ = core::fmt::Write::write_fmt(
                &mut w,
                format_args!("partition {} (unlabelled)", p.number),
            );
            w.len()
        }
    };
    let node = match DeviceNode::try_new_block(
        descriptor,
        geometry,
        BlockKind::Partition,
        &name[..name_len],
        backend,
    ) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // SAFETY: adopt the creation reference.
    let node_ref = unsafe {
        ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode)
    };

    let (first, last) = (p.first_lba, p.first_lba + p.count - 1);
    // 1-based, to agree with the name above and with every other tool. The boot log and a disk
    // list disagreeing about which partition is which is worse than either convention on its own.
    let Some((guid, _)) = gpt else {
        crate::kprintln!(
            "mbr:  partition {} lba {}..{} ({} sectors) type {:#04x} -> block node",
            p.number,
            first,
            last,
            p.count,
            mbr_kind
        );
        crate::device::register_partition(node_ref, disk, "mbr");
        return true;
    };
    // **The label, by name.** A partition is found by its label (`init.toml`'s
    // `gpt-partlabel:`), and until Phase 5 Part C no line said which labels a disk carried — so a
    // live boot could not show it had found `nitrox-live`, and the laptop's hardware report could
    // not list what is on its own disk.
    let label = by_partlabel
        .as_ref()
        .and_then(|p| core::str::from_utf8(&p[PARTLABEL_PREFIX.len()..]).ok())
        .unwrap_or("");
    crate::kprintln!(
        "gpt:  partition {} lba {}..{} ({} sectors) label \"{}\" -> block node",
        p.number,
        first,
        last,
        p.count,
        label
    );
    // A boot disk's names, for `init`; a USB disk's partitions get none (Phase 6 Part D).
    if names == Names::Bind
        && let Some(uuid) = format_partuuid(&guid[..])
    {
        record(PartEntry { node: node_ref.clone(), by_partuuid: uuid, by_partlabel });
    }
    // The device table owns the node (it now also resolves at /dev/blk/<n>), and records that it
    // belongs to `disk`.
    crate::device::register_partition(node_ref, disk, "gpt");
    true
}

/// Append a discovered partition to the registry.
fn record(entry: PartEntry) {
    if PARTITIONS.lock().try_push(entry).is_err() {
        crate::kprintln!("gpt: partition registry full");
    }
}

/// Bind every discovered partition's `/dev/disk/by-partuuid/<uuid>` and
/// `/dev/disk/by-partlabel/<label>` into `ns` as direct handles. Called once when
/// init's root namespace is built (the supervisor). Read-only (`READ` + generic
/// band) — uniform with `/dev/blk`.
pub fn bind_partition_names(ns: &Namespace) {
    // READ + WRITE (the RW fs-server writes filesystem metadata to its partition) + the
    // generic band (DUPLICATE lets it hand a device copy to the kernel for the data path).
    let rights =
        Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    // Snapshot under the lock (clone refs + copy path bytes), then bind without
    // holding the registry lock across the namespace lock.
    let mut snapshot: KVec<(ObjectRef, KVec<u8>, Option<KVec<u8>>)> = KVec::new();
    {
        let parts = PARTITIONS.lock();
        if snapshot.try_reserve(parts.len()).is_err() {
            return;
        }
        for pe in parts.iter() {
            let uuid = copy_bytes(&pe.by_partuuid);
            let label = pe.by_partlabel.as_ref().and_then(copy_bytes_opt);
            if let Some(uuid) = uuid {
                snapshot
                    .try_push((pe.node.clone(), uuid, label))
                    .expect("within reserved capacity");
            }
        }
    }
    for (node, uuid, label) in snapshot.iter() {
        bind_one(ns, uuid, node, rights);
        if let Some(label) = label {
            bind_one(ns, label, node, rights);
        }
    }
}

/// Bind `node` at `path` in `ns`; drop the handed-back ref on failure (outside any
/// lock — `bind` returns it on error).
fn bind_one(ns: &Namespace, path: &KVec<u8>, node: &ObjectRef, rights: Rights) {
    if let Err((reclaimed, _)) = ns.bind(path, node.clone(), rights) {
        drop(reclaimed);
        crate::kprintln!("gpt: binding a /dev/disk name failed");
    }
}

// --- byte helpers -----------------------------------------------------------

/// Copy a `KVec<u8>`'s bytes into a fresh one (`None` on OOM).
fn copy_bytes(src: &KVec<u8>) -> Option<KVec<u8>> {
    let mut out = KVec::new();
    out.try_extend_from_slice(&src[..]).ok()?;
    Some(out)
}

fn copy_bytes_opt(src: &KVec<u8>) -> Option<KVec<u8>> {
    copy_bytes(src)
}

/// Format a GPT partition GUID (mixed-endian: first three fields little-endian,
/// last two big-endian) as `/dev/disk/by-partuuid/<uuid>`. `None` on OOM.
fn format_partuuid(guid: &[u8]) -> Option<KVec<u8>> {
    let mut p = KVec::new();
    p.try_extend_from_slice(b"/dev/disk/by-partuuid/").ok()?;
    // Field byte order for the canonical string form.
    const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    const DASH_AFTER: [usize; 4] = [3, 5, 7, 9]; // positions in ORDER to follow with '-'
    for (i, &o) in ORDER.iter().enumerate() {
        let byte = guid[o];
        p.try_push(hex_lo(byte >> 4)).ok()?;
        p.try_push(hex_lo(byte & 0xF)).ok()?;
        if DASH_AFTER.contains(&i) {
            p.try_push(b'-').ok()?;
        }
    }
    Some(p)
}

fn hex_lo(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'a' + (n - 10) }
}

/// The namespace directory a partition's label is bound under.
const PARTLABEL_PREFIX: &[u8] = b"/dev/disk/by-partlabel/";

/// Decode a GPT partition name (72 bytes UTF-16LE) into
/// `/dev/disk/by-partlabel/<label>`. ASCII-only; `None` if empty, non-ASCII, or
/// containing a path separator (not a usable component).
fn decode_partlabel(name: &[u8]) -> Option<KVec<u8>> {
    let mut p = KVec::new();
    p.try_extend_from_slice(PARTLABEL_PREFIX).ok()?;
    let mut any = false;
    let mut i = 0;
    while i + 1 < name.len() {
        let lo = name[i];
        let hi = name[i + 1];
        if lo == 0 && hi == 0 {
            break; // NUL terminator
        }
        if hi != 0 || lo < 0x20 || lo == b'/' || lo == 0x7f {
            return None; // non-ASCII / control / path separator
        }
        p.try_push(lo).ok()?;
        any = true;
        i += 2;
    }
    if any { Some(p) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn format_partuuid_mixed_endian() {
        init_global_heap();
        // bytes 0..16 of a GUID; canonical string swaps the first three fields.
        let guid = [
            0x78, 0x56, 0x34, 0x12, // time_low (LE) -> 12345678
            0xbc, 0x9a, // time_mid (LE) -> 9abc
            0xf0, 0xde, // time_hi  (LE) -> def0
            0x12, 0x34, // clock_seq (BE) -> 1234
            0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, // node (BE)
        ];
        let p = format_partuuid(&guid).unwrap();
        assert_eq!(
            &p[..],
            &b"/dev/disk/by-partuuid/12345678-9abc-def0-1234-56789abcdef0"[..]
        );
    }

    #[test]
    fn decode_partlabel_ascii() {
        init_global_heap();
        // "ESP" in UTF-16LE, NUL-padded.
        let mut name = [0u8; 72];
        name[0] = b'E';
        name[2] = b'S';
        name[4] = b'P';
        let p = decode_partlabel(&name).unwrap();
        assert_eq!(&p[..], &b"/dev/disk/by-partlabel/ESP"[..]);
    }

    #[test]
    fn decode_partlabel_rejects_empty_and_nonascii() {
        init_global_heap();
        assert!(decode_partlabel(&[0u8; 72]).is_none());
        let mut bad = [0u8; 72];
        bad[1] = 0x01; // high byte set => non-ASCII
        bad[0] = b'x';
        assert!(decode_partlabel(&bad).is_none());
    }
}
