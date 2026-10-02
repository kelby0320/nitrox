//! **Device contexts and input contexts** (xHCI 1.2 §6.2), laid out for the controller's entry
//! size: 32 bytes, or 64 when `HCCPARAMS1.CSZ` says so. QEMU's are 32; the laptop's may be 64.
//!
//! An **output** (device) context is 32 entries the controller owns: the slot context, then one
//! per endpoint. An **input** context is what a command hands it: an input control context saying
//! which entries to add, then the same 32. Everything here is offsets and dwords into a byte
//! slice, so the host tests check them against the specification's layout at both sizes.

/// Port speeds as `PORTSC` and the slot context give them: the default Protocol Speed IDs (xHCI
/// 1.2 §7.2.2.1.1), which apply when a Supported Protocol capability lists none of its own.
pub mod speed {
    /// Full speed, 12 Mb/s.
    pub const FULL: u8 = 1;
    /// Low speed, 1.5 Mb/s.
    pub const LOW: u8 = 2;
    /// High speed, 480 Mb/s.
    pub const HIGH: u8 = 3;
    /// SuperSpeed, 5 Gb/s.
    pub const SUPER: u8 = 4;
    /// SuperSpeedPlus, 10 Gb/s.
    pub const SUPER_PLUS: u8 = 5;

    /// What the log calls `id`.
    pub fn name(id: u8) -> &'static str {
        match id {
            FULL => "full-speed",
            LOW => "low-speed",
            HIGH => "high-speed",
            SUPER => "SuperSpeed",
            SUPER_PLUS => "SuperSpeedPlus",
            _ => "an unknown speed",
        }
    }

    /// Whether `id` is SuperSpeed or faster: where `bMaxPacketSize0` is an exponent.
    pub fn is_super(id: u8) -> bool {
        id == SUPER || id == SUPER_PLUS
    }

    /// The default endpoint's maximum packet before the device has said: 8 at low and full speed
    /// (every device takes 8), 64 at high speed, 512 at SuperSpeed.
    pub fn default_max_packet0(id: u8) -> u16 {
        match id {
            HIGH => 64,
            SUPER | SUPER_PLUS => 512,
            _ => 8,
        }
    }
}

/// The default endpoint's Device Context Index: the doorbell target and the Transfer Event's
/// endpoint ID for it.
pub const DCI_EP0: u8 = 1;

/// Where an input context's entries are, for one entry size.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    entry: usize,
}

impl Layout {
    /// The layout for 64-byte entries when `csz`, 32-byte otherwise.
    pub const fn new(csz: bool) -> Layout {
        Layout { entry: if csz { 64 } else { 32 } }
    }

    /// An input context's bytes: the control context and 32 entries.
    pub const fn input_len(&self) -> usize {
        self.entry * 33
    }

    /// An output context's bytes: 32 entries.
    pub const fn output_len(&self) -> usize {
        self.entry * 32
    }

    /// The input slot context's offset: the entry after the control context.
    pub const fn slot(&self) -> usize {
        self.entry
    }

    /// The input context offset of endpoint `dci`.
    pub const fn endpoint(&self, dci: u8) -> usize {
        self.entry * (1 + dci as usize)
    }
}

/// Write dword `val` at byte offset `off`.
fn put(ctx: &mut [u8], off: usize, val: u32) {
    ctx[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

/// Input Control Context, dword 1: the Add Context flags. `A0` is the slot, `A1` the default
/// endpoint.
const ADD_SLOT: u32 = 1 << 0;
const ADD_EP0: u32 = 1 << 1;
/// Endpoint type 4, Control, in an endpoint context's dword 1 bits 5:3.
const EP_TYPE_CONTROL: u32 = 4 << 3;
/// Error count 3, in dword 1 bits 2:1: retry a failed transaction three times.
const CERR_3: u32 = 3 << 1;
/// A control endpoint's average TRB length, which the specification says to make 8.
const CONTROL_AVERAGE_TRB: u32 = 8;

/// **The input context for Address Device**: the slot — its speed, one context entry, its root
/// port — and the default endpoint, with its transfer ring at `ring`, that ring's cycle state, and
/// `max_packet` as the speed's default. `ctx` must be `input_len` bytes, zeroed.
pub fn address_device(ctx: &mut [u8], l: Layout, port: u8, speed_id: u8, ring: u64, cycle: bool, max_packet: u16) {
    put(ctx, 4, ADD_SLOT | ADD_EP0);
    // Slot, dword 0: route string 0 (a root port), the speed in bits 23:20, context entries 1 in
    // bits 31:27. Dword 1: the root hub port number in bits 23:16.
    put(ctx, l.slot(), (speed_id as u32) << 20 | 1 << 27);
    put(ctx, l.slot() + 4, (port as u32) << 16);
    endpoint0(ctx, l, ring, cycle, max_packet);
}

/// **The input context for Evaluate Context** of the default endpoint: only `A1`, and the endpoint
/// with its new maximum packet. `ctx` must be `input_len` bytes, zeroed.
pub fn evaluate_ep0(ctx: &mut [u8], l: Layout, ring: u64, cycle: bool, max_packet: u16) {
    put(ctx, 4, ADD_EP0);
    endpoint0(ctx, l, ring, cycle, max_packet);
}

/// The default endpoint's context: control, three retries, `max_packet` in dword 1 bits 31:16,
/// the dequeue pointer and its cycle state in dwords 2 and 3, and the average TRB length.
fn endpoint0(ctx: &mut [u8], l: Layout, ring: u64, cycle: bool, max_packet: u16) {
    let ep = l.endpoint(DCI_EP0);
    put(ctx, ep + 4, CERR_3 | EP_TYPE_CONTROL | (max_packet as u32) << 16);
    put(ctx, ep + 8, (ring as u32 & !0xF) | cycle as u32);
    put(ctx, ep + 12, (ring >> 32) as u32);
    put(ctx, ep + 16, CONTROL_AVERAGE_TRB);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dword(ctx: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(ctx[off..off + 4].try_into().unwrap())
    }

    /// **The same Address Device context at both entry sizes**, each field where the specification
    /// puts it: the slot at entry 1, the default endpoint at entry 2.
    #[test]
    fn address_device_lays_out_the_slot_and_ep0_at_either_size() {
        for (csz, entry) in [(false, 32), (true, 64)] {
            let l = Layout::new(csz);
            assert_eq!((l.input_len(), l.output_len()), (33 * entry, 32 * entry));
            let mut ctx = vec![0u8; l.input_len()];
            address_device(&mut ctx, l, 13, speed::HIGH, 0x1_2345_6000, true, 64);
            assert_eq!(dword(&ctx, 0), 0, "nothing dropped");
            assert_eq!(dword(&ctx, 4), 0b11, "A0 and A1");
            assert_eq!(dword(&ctx, entry), 3 << 20 | 1 << 27, "high speed, one entry, entry {entry}");
            assert_eq!(dword(&ctx, entry + 4), 13 << 16, "root port 13");
            let ep = 2 * entry;
            assert_eq!(dword(&ctx, ep), 0);
            assert_eq!(dword(&ctx, ep + 4), 64 << 16 | 4 << 3 | 3 << 1, "control, CErr 3, 64 bytes");
            assert_eq!(dword(&ctx, ep + 8), 0x2345_6001, "the ring's low half, with DCS");
            assert_eq!(dword(&ctx, ep + 12), 0x1);
            assert_eq!(dword(&ctx, ep + 16), 8);
            assert!(ctx[ep + 20..].iter().all(|&b| b == 0), "nothing past the default endpoint");
        }
    }

    /// Evaluate Context names the default endpoint alone, and leaves the slot entry empty.
    #[test]
    fn evaluate_names_only_the_default_endpoint() {
        let l = Layout::new(false);
        let mut ctx = vec![0u8; l.input_len()];
        evaluate_ep0(&mut ctx, l, 0x8000, false, 8);
        assert_eq!(dword(&ctx, 4), 0b10, "A1 only");
        assert!(ctx[32..64].iter().all(|&b| b == 0), "the slot context is not written");
        assert_eq!(dword(&ctx, 64 + 4), 8 << 16 | 4 << 3 | 3 << 1);
        assert_eq!(dword(&ctx, 64 + 8), 0x8000, "cycle state 0");
    }

    #[test]
    fn each_speed_has_its_default_packet_and_its_name() {
        assert_eq!(speed::default_max_packet0(speed::LOW), 8);
        assert_eq!(speed::default_max_packet0(speed::FULL), 8);
        assert_eq!(speed::default_max_packet0(speed::HIGH), 64);
        assert_eq!(speed::default_max_packet0(speed::SUPER), 512);
        assert!(speed::is_super(speed::SUPER_PLUS) && !speed::is_super(speed::HIGH));
        assert_eq!(speed::name(speed::FULL), "full-speed");
        assert_eq!(speed::name(9), "an unknown speed");
    }
}
