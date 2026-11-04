use super::*;

/// The one memory object every device read passes through, mapped for the life of the service.
pub(super) struct Scratch {
    mem: Handle<Memory, MapReadWrite>,
    map: Mapping<MapReadWrite>,
}

impl Scratch {
    pub(super) fn new() -> Option<Scratch> {
        let mem = Handle::<Memory, MapReadWrite>::create(SCRATCH).ok()?;
        let map = mem.map(SCRATCH).ok()?;
        Some(Scratch { mem, map })
    }
}

pub(super) struct DeviceIo<'a> {
    node: &'a Node,
    len: u64,
    sector: u64,
    scratch: &'a Scratch,
}

impl<'a> DeviceIo<'a> {
    pub(super) fn new(node: &'a Node, r: &DeviceRecord, scratch: &'a Scratch) -> DeviceIo<'a> {
        let sector = if r.logical_block_size == 0 { 512 } else { r.logical_block_size as u64 };
        DeviceIo { node, len: sector.saturating_mul(r.block_count), sector, scratch }
    }
}

impl BlockReader for DeviceIo<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let end = offset.checked_add(buf.len() as u64).ok_or(FsError::Io)?;
        if end > self.len {
            return Err(FsError::Io);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let ScratchRead { start, intra, want, span } =
                scratch_read(offset + done as u64, buf.len() - done, self.sector).ok_or(FsError::Io)?;
            let moved = block_on(self.node.read(&self.scratch.mem, 0, start, span));
            if moved.ok() != Some(span as usize) {
                return Err(FsError::Io);
            }
            // `scratch_read` keeps `intra + want <= span <= SCRATCH`.
            self.scratch.map.read_into(intra, &mut buf[done..done + want]).map_err(|_| FsError::Io)?;
            done += want;
        }
        Ok(())
    }
}

/// Take `block`: every device queued before `Settled`. `Err` with why if the class was not taken.
#[allow(clippy::type_complexity, reason = "the pair is returned once and a name for it would only be read here")]
pub(super) fn take_block(root: &Root) -> Result<(Chan, Vec<(DeviceRecord, Node)>), &'static [u8]> {
    let sub = match block_on(root.lookup::<Channel, Only>(SUBSCRIPTION, Rights::SEND | Rights::RECV | Rights::WAIT)) {
        Ok(sub) => sub,
        Err(e) if e.code() == KError::AlreadyExists => {
            return Err(b"block is already owned, so the disks are someone else's");
        }
        Err(_) => return Err(b"/svc/devices/block did not resolve, so there are no disks to serve"),
    };
    let mut devices = Vec::new();
    loop {
        let (m, mut sent) = match recv(&sub) {
            Ok(Some(m)) => m,
            Ok(None) if wait_until(&sub, within(REPLAY_WAIT_NS)) => continue,
            _ => return Err(b"the device manager's replay stopped before Settled"),
        };
        match (m.op, parse_arrived(&m.body).and_then(DeviceRecord::read), sent.len()) {
            (OP_DEVICES_SETTLED, _, _) => return Ok((sub, devices)),
            (OP_DEVICES_ARRIVED, Some(r), 1) if let Some(node) = sent.take(0) => devices.push((r, node)),
            _ => {}
        }
    }
}

pub(super) fn read_table(io: &DeviceIo, disk: u32) -> Option<DiskTable> {
    let mut front = alloc::vec![0u8; libgpt::table::FRONT_BYTES];
    io.read_at(0, &mut front).ok()?;
    let t = libgpt::table::read(&front).ok()?;
    let entries = t
        .partitions()
        .iter()
        .map(|p| TableEntry { guid: p.unique_guid, name: p.name().to_vec(), blocks: p.blocks() })
        .collect();
    Some(DiskTable { disk, entries })
}

/// `init`'s manifest, or why it could not be read: each way it can fail says so.
pub(super) fn read_manifest(root: &Root) -> Result<manifest::Manifest, &'static [u8]> {
    // `INSPECT` as well as `MAP_READ`: the object's size comes from `stat`.
    let Ok(obj) = block_on(root.lookup::<File, MapRead>(INIT_TOML, Rights::MAP_READ | Rights::INSPECT)) else {
        return Err(b"it did not resolve");
    };
    let wont_map = b"it would not map";
    let size = match obj.stat() {
        Ok(info) if info.size != 0 => info.size as usize,
        _ => return Err(wont_map),
    };
    // Reserved fallibly: an absurd size is a manifest that does not map, not a heap panic.
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| wont_map.as_slice())?;
    bytes.resize(size, 0);
    obj.map(size).and_then(|view| view.read_into(0, &mut bytes)).map_err(|_| wont_map.as_slice())?;
    match manifest::text(&bytes) {
        Ok(text) => manifest::parse(text).map_err(|_| b"it did not parse".as_slice()),
        Err(_) => Err(b"it is not UTF-8"),
    }
}
