//! **A disk's partitions, published** — GPT's and MBR's.
//!
//! [`partitions`](super::partitions) parses the table; this publishes each partition as a block
//! [`DeviceNode`] over a [`Partition`](crate::io::block::Partition) window — the first
//! **two-layer** block IRP stack (partition rebases the offset and forwards to the disk),
//! registered in the device table (`/dev/blk/<n>`). **A boot disk's GPT partitions** are also
//! recorded for the `/dev/disk/by-partuuid/<uuid>` and `by-partlabel/<label>` bindings made by
//! [`bind_partition_names`]; **a USB disk's are not** ([`Names`]), since those names are how `init`
//! finds its critical path among what the boot's probe found. The disk whose GPT GUID is the one
//! Limine loaded the modules from is flagged `BOOT` ([`partitions::is_boot`]), and **a name two
//! partitions carry goes to the boot medium's** ([`bind_order`]).
//!
//! [`init`] reads with interrupts masked, so it uses the polled
//! [`read_blocking`](crate::io::block::read_blocking). The signature, entry size and bounds are
//! validated; the header and array CRCs are not (`TODO(gpt-crc)`).

use super::partitions::{self, PartKind, Scheme, Table, Unread};
use crate::io::block::{Partition, read_blocking};
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME, NameBuf};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{KBox, KVec, Rights, SpinLock};
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::{Namespace, ObjectRef};

const SECTOR: u64 = partitions::BLOCK as u64;

struct PartEntry {
    node: ObjectRef,
    by_partuuid: KVec<u8>,
    /// `/dev/disk/by-partlabel/<label>` path (absent if the label is not a usable path component).
    by_partlabel: Option<KVec<u8>>,
    /// On the disk the machine started from, or a RAM disk, which only the boot medium's modules
    /// are.
    boot_medium: bool,
}

/// Partitions discovered across all disks, for [`bind_partition_names`]; written by [`init`].
static PARTITIONS: SpinLock<KVec<PartEntry>> = SpinLock::new(LockRank::Registry, KVec::new());

/// Whether a disk's partitions are recorded for `/dev/disk`.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Names {
    /// A disk the boot's probe found: its GPT partitions are recorded for [`bind_partition_names`].
    Bind,
    /// A disk that arrived later, a USB one: none are.
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
    // SAFETY: `disk` pins a live `DeviceNode`.
    let mut boot_medium =
        unsafe { &*(disk.as_ptr() as *const DeviceNode) }.block_info().kind == BlockKind::RamDisk.as_u32();
    let what = match table.scheme {
        Scheme::Gpt { disk_guid } => {
            let boot = partitions::is_boot(&disk_guid, partitions::boot_disk());
            if boot && crate::device::mark_boot(disk) {
                crate::kprintln!("gpt: the disk the machine started from: its GUID is the one Limine loaded from");
            }
            boot_medium |= boot;
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
        if publish_partition(disk, p, names, boot_medium) {
            found += 1;
        }
    }
    crate::kprintln!("{what}: {} partition(s)", found);
    if table.extended > 0 {
        crate::kprintln!(
            "mbr: {} extended partition(s) passed over: their logical partitions are not read",
            table.extended
        );
    }
    found
}

/// Create a partition window and block `DeviceNode`, register it, and record a boot disk's GPT
/// names.
fn publish_partition(disk: &ObjectRef, p: &partitions::Part, names: Names, boot_medium: bool) -> bool {
    let Some(part) = Partition::new(disk, p.first_lba, p.count, SECTOR) else {
        return false;
    };
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
    let geometry = BlockGeometry { logical_block_size: SECTOR as u32, block_count: p.count };
    let (gpt, mbr_kind) = match &p.kind {
        PartKind::Gpt { guid, name } => (Some((guid, name)), 0),
        PartKind::Mbr { kind } => (None, *kind),
    };
    let by_partlabel = gpt.and_then(|(_, name)| decode_partlabel(&name[..]));
    // **A partition's name is its label** — what `init.toml` selects it by. An unlabelled one says
    // which slice of which disk it is: "partition" alone cannot be confirmed before destroying it.
    let mut name = [0u8; MAX_DEVICE_NAME];
    let name_len = match by_partlabel.as_ref() {
        // `decode_partlabel` returns the whole `/dev/disk/by-partlabel/<label>` path; a device's
        // name is the label itself, which a person can read and type.
        Some(path) => {
            let label = path.strip_prefix(PARTLABEL_PREFIX).unwrap_or(&path[..]);
            let n = label.len().min(MAX_DEVICE_NAME);
            name[..n].copy_from_slice(&label[..n]);
            n
        }
        None => {
            let mut w = NameBuf::new(&mut name);
            // **Numbered from one, as every other tool numbers partitions** (`/dev/sda1`,
            // `sgdisk`'s listing, the firmware's `HD(1,GPT,…)`); an array position counts from
            // zero.
            let _ = core::fmt::Write::write_fmt(&mut w, format_args!("partition {} (unlabelled)", p.number));
            w.len()
        }
    };
    let node = match DeviceNode::try_new_partition(descriptor, geometry, &name[..name_len], part) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // SAFETY: adopt the creation reference.
    let node_ref = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };

    let (first, last) = (p.first_lba, p.first_lba + p.count - 1);
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
    // **The label, by name**: a partition is found by its label (`init.toml`'s `gpt-partlabel:`),
    // so the boot log and the hardware report must say which labels a disk carries.
    let label =
        by_partlabel.as_ref().and_then(|p| core::str::from_utf8(&p[PARTLABEL_PREFIX.len()..]).ok()).unwrap_or("");
    crate::kprintln!(
        "gpt:  partition {} lba {}..{} ({} sectors) label \"{}\" -> block node",
        p.number,
        first,
        last,
        p.count,
        label
    );
    if names == Names::Bind
        && let Some(uuid) = format_partuuid(&guid[..])
    {
        record(PartEntry { node: node_ref.clone(), by_partuuid: uuid, by_partlabel, boot_medium });
    }
    // The device table owns the node (it also resolves at /dev/blk/<n>) and records its disk.
    crate::device::register_partition(node_ref, disk, "gpt");
    true
}

fn record(entry: PartEntry) {
    if PARTITIONS.lock().try_push(entry).is_err() {
        crate::kprintln!("gpt: partition registry full");
    }
}

/// Bind every discovered partition's `/dev/disk/by-partuuid/<uuid>` and `by-partlabel/<label>` name
/// into `ns` as direct handles. Called once when init's root namespace is built.
pub fn bind_partition_names(ns: &Namespace) {
    let Some(names) = bind_order(&PARTITIONS.lock()) else {
        crate::kprintln!("gpt: no memory for the /dev/disk names");
        return;
    };
    bind_names(ns, &names);
    for (_, path, _) in names.iter().filter(|(_, _, free)| !free) {
        crate::kprintln!(
            "gpt: {} names an earlier partition, the boot medium's or the first found, not this one",
            core::str::from_utf8(path).unwrap_or("a /dev/disk name")
        );
    }
}

/// Bind each name [`bind_order`] left free to its partition.
fn bind_names(ns: &Namespace, names: &[(ObjectRef, KVec<u8>, bool)]) {
    // READ + WRITE (the RW fs-server writes filesystem metadata to its partition) + the
    // generic band (DUPLICATE lets it hand a device copy to the kernel for the data path).
    let rights = Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    for (node, path, _) in names.iter().filter(|(_, _, free)| *free) {
        bind_one(ns, path, node, rights);
    }
}

/// **Which partition each `/dev/disk` name goes to**, and whether it is still free for it: the
/// boot medium's first, so another disk copying a label or GUID it carries never takes it, then
/// the rest, each in the order found. `None` on running out of memory.
fn bind_order(parts: &[PartEntry]) -> Option<KVec<(ObjectRef, KVec<u8>, bool)>> {
    let mut out: KVec<(ObjectRef, KVec<u8>, bool)> = KVec::new();
    out.try_reserve(2 * parts.len()).ok()?;
    for boot_medium in [true, false] {
        for pe in parts.iter().filter(|pe| pe.boot_medium == boot_medium) {
            for path in core::iter::once(&pe.by_partuuid).chain(pe.by_partlabel.as_ref()) {
                let free = !out.iter().any(|(_, taken, free)| *free && taken[..] == path[..]);
                out.try_push((pe.node.clone(), copy_bytes(path)?, free)).ok()?;
            }
        }
    }
    Some(out)
}

/// Bind `node` at `path` in `ns`; drop the handed-back ref on failure (outside any
/// lock — `bind` returns it on error).
fn bind_one(ns: &Namespace, path: &KVec<u8>, node: &ObjectRef, rights: Rights) {
    if let Err((reclaimed, _)) = ns.bind(path, node.clone(), rights) {
        drop(reclaimed);
        crate::kprintln!("gpt: binding a /dev/disk name failed");
    }
}

fn copy_bytes(src: &KVec<u8>) -> Option<KVec<u8>> {
    let mut out = KVec::new();
    out.try_extend_from_slice(&src[..]).ok()?;
    Some(out)
}

/// Format a GPT partition GUID (mixed-endian: first three fields little-endian,
/// last two big-endian) as `/dev/disk/by-partuuid/<uuid>`. `None` on OOM.
fn format_partuuid(guid: &[u8]) -> Option<KVec<u8>> {
    let mut p = KVec::new();
    p.try_extend_from_slice(b"/dev/disk/by-partuuid/").ok()?;
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

const PARTLABEL_PREFIX: &[u8] = b"/dev/disk/by-partlabel/";

/// Decode a GPT partition name (72 bytes UTF-16LE) into `/dev/disk/by-partlabel/<label>`; `None` if
/// empty, non-ASCII, or containing a path separator.
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
        let guid = [
            0x78, 0x56, 0x34, 0x12, // time_low (LE) -> 12345678
            0xbc, 0x9a, // time_mid (LE) -> 9abc
            0xf0, 0xde, // time_hi  (LE) -> def0
            0x12, 0x34, // clock_seq (BE) -> 1234
            0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, // node (BE)
        ];
        let p = format_partuuid(&guid).unwrap();
        assert_eq!(&p[..], &b"/dev/disk/by-partuuid/12345678-9abc-def0-1234-56789abcdef0"[..]);
    }

    #[test]
    fn decode_partlabel_ascii() {
        init_global_heap();
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

    /// The live root, on a RAM disk published after a SATA disk carrying its label and GUID, keeps
    /// both; a name only one partition carries is bound wherever it is.
    #[test]
    fn a_name_two_partitions_carry_goes_to_the_boot_mediums() {
        use crate::object::{MemoryObject, namespace::ResolvedTarget};
        init_global_heap();
        let node = || {
            let mem = KBox::into_raw(MemoryObject::try_new(4096).unwrap()).as_ptr() as *mut ();
            // SAFETY: a fresh `into_raw`, carrying its single creation reference.
            unsafe { ObjectRef::from_raw(mem, KObjectType::MemoryObject) }
        };
        let path = |s: &[u8]| {
            copy_bytes(&{
                let mut v = KVec::new();
                v.try_extend_from_slice(s).unwrap();
                v
            })
        };
        let entry = |node: &ObjectRef, uuid: &[u8], label: &[u8], boot_medium| PartEntry {
            node: node.clone(),
            by_partuuid: path(uuid).unwrap(),
            by_partlabel: path(label),
            boot_medium,
        };
        let (sata_root, sata_other, ram_root) = (node(), node(), node());
        let live = b"/dev/disk/by-partlabel/nitrox-live";
        let uuid = b"/dev/disk/by-partuuid/0000000a-0000-0000-0000-000000000000";
        let parts = [
            entry(&sata_root, uuid, live, false),
            entry(&sata_other, b"/dev/disk/by-partuuid/b", b"/dev/disk/by-partlabel/nitrox-root", false),
            entry(&ram_root, uuid, live, true),
        ];
        let ns = crate::object::Namespace::try_new().unwrap();
        bind_names(&ns, &bind_order(&parts).unwrap());
        let at = |p: &[u8]| match ns.resolve(p) {
            Some((ResolvedTarget::DirectHandle(o), _, _)) => o.as_ptr(),
            _ => core::ptr::null_mut(),
        };
        assert_eq!(at(live), ram_root.as_ptr(), "the label to the RAM disk's root");
        assert_eq!(at(uuid), ram_root.as_ptr(), "and the GUID");
        assert_eq!(at(b"/dev/disk/by-partlabel/nitrox-root"), sata_other.as_ptr(), "a name only one carries");
        let passed: KVec<bool> = {
            let mut v = KVec::new();
            for (_, _, free) in bind_order(&parts).unwrap().iter() {
                v.try_push(*free).unwrap();
            }
            v
        };
        let want = [true, true, false, false, true, true];
        assert_eq!(&passed[..], &want, "the RAM disk's names, the SATA root's passed over, then the other's");
    }
}
