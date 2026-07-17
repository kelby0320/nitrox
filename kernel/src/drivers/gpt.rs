//! GPT (GUID Partition Table) Tier 1 driver.
//!
//! Reads a block disk's GPT (`drivers::probe` calls [`init`] for each disk),
//! and publishes each partition as a block [`DeviceNode`] over a
//! [`Partition`](crate::io::block::Partition) window — the first **two-layer**
//! block IRP stack (partition rebases the offset and forwards to the disk). The
//! partition nodes are registered in the device table (so they appear at
//! `/dev/blk/<n>`) and recorded for the stable `/dev/disk/by-partuuid/<uuid>` and
//! `/dev/disk/by-partlabel/<label>` namespace bindings (created at boot by
//! [`bind_partition_names`]).
//!
//! Phase 2 reads at boot with interrupts masked, so it uses the synchronous
//! polled [`read_blocking`](crate::io::block::read_blocking). GPT header/array
//! CRC validation is deferred (the signature + sane bounds are checked).

use crate::io::block::{Partition, partition_backend, read_blocking};
use crate::libkern::handle::KObjectType;
use crate::libkern::{KBox, KVec, Rights, SpinLock};
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::{Namespace, ObjectRef};

const SECTOR: u64 = 512;
/// GPT header signature ("EFI PART").
const GPT_SIG: &[u8; 8] = b"EFI PART";
/// Cap on partition-array sectors scanned (128 × 128-byte entries) — Phase 2 sanity.
const MAX_ARRAY_SECTORS: u64 = 32;

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
static PARTITIONS: SpinLock<KVec<PartEntry>> = SpinLock::new(KVec::new());

/// Parse `disk`'s GPT and publish its partitions. No-op (with a log) if the disk
/// has no valid GPT. Boot-time, interrupts masked (reads are polled).
pub fn init(disk: &ObjectRef) {
    let mut hdr = [0u8; 512];
    if !read_blocking(disk, 1, 1, &mut hdr) {
        crate::kprintln!("gpt: header read failed");
        return;
    }
    if &hdr[0..8] != GPT_SIG {
        crate::kprintln!("gpt: no GPT (LBA1 not 'EFI PART')");
        return;
    }
    let array_lba = rd_u64(&hdr, 72);
    let num_entries = rd_u32(&hdr, 80);
    let entry_size = rd_u32(&hdr, 84) as usize;
    if entry_size < 128 || entry_size > SECTOR as usize || SECTOR as usize % entry_size != 0 {
        crate::kprintln!("gpt: unsupported entry size {}", entry_size);
        return;
    }
    let per_sector = SECTOR as usize / entry_size;
    let total_sectors = ((num_entries as u64).div_ceil(per_sector as u64)).min(MAX_ARRAY_SECTORS);

    let mut sector = [0u8; 512];
    let mut found = 0u32;
    for s in 0..total_sectors {
        if !read_blocking(disk, array_lba + s, 1, &mut sector) {
            break;
        }
        for k in 0..per_sector {
            let idx = s as usize * per_sector + k;
            if idx >= num_entries as usize {
                break;
            }
            let e = &sector[k * entry_size..k * entry_size + 128];
            // Type GUID all-zero ⇒ unused entry.
            if e[0..16].iter().all(|&b| b == 0) {
                continue;
            }
            let first = rd_u64(e, 32);
            let last = rd_u64(e, 40);
            if last < first {
                continue;
            }
            let count = last - first + 1;
            if publish_partition(disk, e, first, count, found) {
                found += 1;
            }
        }
    }
    crate::kprintln!("gpt: {} partition(s)", found);
}

/// Create a partition window + block `DeviceNode`, register it, and record its
/// `by-partuuid`/`by-partlabel` paths. `e` is the 128-byte GPT entry.
fn publish_partition(disk: &ObjectRef, e: &[u8], first_lba: u64, count: u64, index: u32) -> bool {
    let Some(part) = Partition::new(disk, first_lba, count, SECTOR) else {
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
        block_count: count,
    };
    let node = match DeviceNode::try_new_block(descriptor, geometry, backend) {
        Ok(n) => n,
        Err(_) => return false,
    };
    // SAFETY: adopt the creation reference.
    let node_ref = unsafe {
        ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode)
    };

    let by_partuuid = format_partuuid(&e[16..32]);
    let by_partlabel = decode_partlabel(&e[56..128]);
    if let Some(uuid) = by_partuuid {
        record(PartEntry {
            node: node_ref.clone(),
            by_partuuid: uuid,
            by_partlabel,
        });
    }
    crate::kprintln!(
        "gpt:  partition {} lba {}..{} ({} sectors) -> block node",
        index,
        first_lba,
        first_lba + count - 1,
        count
    );
    // The device table owns the node (it now also resolves at /dev/blk/<n>).
    crate::device::register(node_ref);
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

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd_u64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

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

/// Decode a GPT partition name (72 bytes UTF-16LE) into
/// `/dev/disk/by-partlabel/<label>`. ASCII-only; `None` if empty, non-ASCII, or
/// containing a path separator (not a usable component).
fn decode_partlabel(name: &[u8]) -> Option<KVec<u8>> {
    let mut p = KVec::new();
    p.try_extend_from_slice(b"/dev/disk/by-partlabel/").ok()?;
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
