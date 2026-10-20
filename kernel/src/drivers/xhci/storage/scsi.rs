/// `TEST UNIT READY`.
pub const fn test_unit_ready() -> [u8; 6] {
    [0x00, 0, 0, 0, 0, 0]
}

/// `REQUEST SENSE` for 18 bytes of fixed-format sense data.
pub const fn request_sense() -> [u8; 6] {
    [0x03, 0, 0, 0, SENSE_LEN as u8, 0]
}

/// `INQUIRY` for the standard data's first 36 bytes.
pub const fn inquiry() -> [u8; 6] {
    [0x12, 0, 0, 0, INQUIRY_LEN as u8, 0]
}

/// `READ CAPACITY(10)`.
pub const fn read_capacity() -> [u8; 10] {
    [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// `READ(10)` or `WRITE(10)` of `count` blocks from `lba`, big-endian.
pub fn rw10(write: bool, lba: u32, count: u16) -> [u8; 10] {
    let l = lba.to_be_bytes();
    let c = count.to_be_bytes();
    [if write { 0x2A } else { 0x28 }, 0, l[0], l[1], l[2], l[3], 0, c[0], c[1], 0]
}

/// `READ CAPACITY(16)` — `SERVICE ACTION IN(16)`, service action `0x10` — for its answer's
/// first [`CAPACITY16_LEN`] bytes: a unit too big for `READ CAPACITY(10)`.
pub const fn read_capacity16() -> [u8; 16] {
    [0x9E, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, CAPACITY16_LEN as u8, 0, 0]
}

/// `READ(16)` or `WRITE(16)` of `count` blocks from `lba`, big-endian: a unit of 2³² blocks or
/// more.
pub fn rw16(write: bool, lba: u64, count: u32) -> [u8; 16] {
    let l = lba.to_be_bytes();
    let c = count.to_be_bytes();
    [if write { 0x8A } else { 0x88 }, 0, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], c[0], c[1], c[2], c[3], 0, 0]
}

/// `SYNCHRONIZE CACHE(10)` of the whole medium: an LBA and a count of zero.
pub const fn synchronize_cache() -> [u8; 10] {
    [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

/// `START STOP UNIT` with LoEj set and Start clear, as util-linux's `eject` sends.
pub const fn eject() -> [u8; 6] {
    [0x1B, 0, 0, 0, 0x02, 0]
}

/// Fixed-format sense data's length, as `REQUEST SENSE` asks for it.
pub const SENSE_LEN: usize = 18;
/// Standard `INQUIRY` data's length, through the product revision.
pub const INQUIRY_LEN: usize = 36;
/// `READ CAPACITY(10)`'s answer: the last block's address and the block length.
pub const CAPACITY_LEN: usize = 8;
/// How much of `READ CAPACITY(16)`'s answer is asked for: its parameter data's 32 bytes, of
/// which the first twelve are the last block's address and the block length.
pub const CAPACITY16_LEN: usize = 32;

/// Sense key: the unit is not ready — becoming so, or with no medium.
pub const NOT_READY: u8 = 0x2;
/// Sense key: the command was refused — for a flush, there is no cache to flush.
pub const ILLEGAL_REQUEST: u8 = 0x5;
/// Sense key: the unit was reset or its medium changed, and says so once.
pub const UNIT_ATTENTION: u8 = 0x6;
/// The additional sense code for "medium not present", under `NOT_READY`.
pub const MEDIUM_NOT_PRESENT: u8 = 0x3A;

/// What `INQUIRY` says of a unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// A direct-access block device, connected: qualifier 0, type 0.
    pub direct_access: bool,
    /// The vendor's identification, space-padded ASCII.
    pub vendor: [u8; 8],
    /// The product's identification, space-padded ASCII.
    pub product: [u8; 16],
}

/// Read `INQUIRY`'s answer, or `None` if it is too short to be one.
pub fn inquiry_answer(b: &[u8]) -> Option<Inquiry> {
    if b.len() < INQUIRY_LEN {
        return None;
    }
    let mut vendor = [0u8; 8];
    vendor.copy_from_slice(&b[8..16]);
    let mut product = [0u8; 16];
    product.copy_from_slice(&b[16..32]);
    Some(Inquiry { direct_access: b[0] == 0x00, vendor, product })
}

/// **What `READ CAPACITY(10)` says**: the block count and size, or `None` for a unit of 2 TiB or
/// more — its last LBA reads `0xFFFFFFFF`, which the ten-byte commands cannot address past.
pub fn capacity_answer(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < CAPACITY_LEN {
        return None;
    }
    let last = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let block = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    (last != u32::MAX).then_some((last as u64 + 1, block))
}

/// **What `READ CAPACITY(16)` says**: the block count and size, or `None` for an answer too short
/// to hold them, or one naming no block at all.
pub fn capacity16_answer(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < 12 {
        return None;
    }
    let last = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let block = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
    last.checked_add(1).map(|blocks| (blocks, block))
}

/// Fixed-format sense data's key, additional sense code and qualifier.
pub fn sense_answer(b: &[u8]) -> (u8, u8, u8) {
    if b.len() < 14 {
        return (0, 0, 0);
    }
    (b[2] & 0xF, b[12], b[13])
}
