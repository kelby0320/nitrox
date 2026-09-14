//! x86_64 ACPI static-table parser ([`ArchPlatform`] impl).
//!
//! Pure-Rust, **no AML**: walk the RSDP → RSDT/XSDT → the two tables Phase 2
//! needs — the **MADT** (interrupt routing: IOAPIC bases, GSI bases, ISA-IRQ
//! source overrides) and the **MCFG** (PCIe ECAM windows). ACPICA/AML is a
//! separate, deferred concern (see `docs/rationale/why-phased-acpi.md`).
//!
//! ## Arch boundary
//!
//! Only the **PCIe ECAM regions** cross into neutral code, through
//! [`ArchPlatform::pcie_ecam_regions`] — PCIe config space is a PCI-SIG
//! standard, identical across architectures. The MADT-derived interrupt-routing
//! facts are **arch-internal**: cached here and read directly by the x86 IOAPIC
//! code (`pub(crate)` [`ioapics`] / [`source_overrides`]). IOAPIC / GSI / MADT
//! never appear in neutral names. See `docs/architecture/drivers-and-irps.md`
//! and the decision log (2026-06-11).
//!
//! ## Structure
//!
//! The byte-level parsers ([`parse_rsdp`], [`parse_madt`], [`parse_mcfg`],
//! [`sdt_pointers`]) are pure functions over `&[u8]`, host-tested against
//! synthetic table blobs. The only non-pure part is the boot glue in
//! [`X86Platform::init`]: read the RSDP from Limine, translate physical table
//! addresses through the HHDM, and feed the bytes to the parsers.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::platform::{ArchPlatform, EcamRegion};
use crate::libkern::AllocError;
use crate::libkern::printable::Printable;
use crate::mm::{PhysAddr, heap};

// --- Limine RSDP request (x86 firmware detail; owned here so `init` is arg-free) ---
//
// Lives in `.limine_requests` like the requests in `main.rs`; the linker
// collects this section from every object between the start/end markers, so a
// request declared in a submodule is discovered by the bootloader all the same.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut RSDP_REQUEST: crate::limine::RsdpRequest = crate::limine::RsdpRequest::new();

// --- Caps on the fixed static caches (no dynamic allocation) ---------------
const MAX_ECAM: usize = 8;
const MAX_IOAPIC: usize = 8;
const MAX_OVERRIDE: usize = 24; // ISA exposes 16 IRQ lines; overrides ≤ that
const MAX_CPU: usize = 64;
/// Defensive cap on a single table's byte length (real ACPI tables are tiny);
/// stops a corrupt length field from forming a slice past mapped memory.
const MAX_TABLE_BYTES: usize = 0x1_0000;

/// One I/O APIC discovered from the MADT (arch-internal — consumed by the x86
/// IOAPIC bring-up, never crosses the arch boundary).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct IoApic {
    pub id: u8,
    pub addr: PhysAddr,
    pub gsi_base: u32,
}

impl IoApic {
    const ZERO: IoApic = IoApic {
        id: 0,
        addr: PhysAddr(0),
        gsi_base: 0,
    };
}

/// An ISA-IRQ → GSI interrupt source override from the MADT (arch-internal).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceOverride {
    /// The legacy ISA IRQ line (0–15) that is remapped.
    pub source_irq: u8,
    /// The Global System Interrupt it is delivered on instead.
    pub gsi: u32,
    /// MPS INTI flags (polarity bits 1:0, trigger bits 3:2).
    pub flags: u16,
}

impl SourceOverride {
    const ZERO: SourceOverride = SourceOverride {
        source_irq: 0,
        gsi: 0,
        flags: 0,
    };
}

// --- Static caches: written once by `init` at boot, read-only thereafter ---
static ECAM_COUNT: AtomicUsize = AtomicUsize::new(0);
static mut ECAM: [EcamRegion; MAX_ECAM] = [EcamRegion::ZERO; MAX_ECAM];
static IOAPIC_COUNT: AtomicUsize = AtomicUsize::new(0);
static mut IOAPICS: [IoApic; MAX_IOAPIC] = [IoApic::ZERO; MAX_IOAPIC];
static OVERRIDE_COUNT: AtomicUsize = AtomicUsize::new(0);
static mut OVERRIDES: [SourceOverride; MAX_OVERRIDE] = [SourceOverride::ZERO; MAX_OVERRIDE];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);
static mut CPU_APIC_IDS: [u32; MAX_CPU] = [0; MAX_CPU];

// --- Little-endian field readers (callers guarantee `off + width <= b.len()`) ---
#[inline]
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// ACPI checksum: the bytes sum to zero (mod 256).
fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

/// The common 36-byte System Description Table header is present.
fn sdt_signature(b: &[u8]) -> Option<[u8; 4]> {
    if b.len() < 36 {
        None
    } else {
        Some([b[0], b[1], b[2], b[3]])
    }
}

/// Total length of an SDT (header field at offset 4). Caller must have ≥ 36 bytes.
fn sdt_length(b: &[u8]) -> usize {
    rd_u32(b, 4) as usize
}

/// What [`parse_rsdp`] extracts: which root table to walk, and at what address.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct RsdpInfo {
    revision: u8,
    sdt_phys: PhysAddr,
    use_xsdt: bool,
}

/// Validate an RSDP (Root System Description Pointer) and extract the root SDT
/// address. Prefers the 64-bit XSDT (ACPI 2.0+) when present and valid, else the
/// 32-bit RSDT. Returns `None` on a bad signature or checksum.
fn parse_rsdp(rsdp: &[u8]) -> Option<RsdpInfo> {
    if rsdp.len() < 20 || &rsdp[0..8] != b"RSD PTR " || !checksum_ok(&rsdp[0..20]) {
        return None;
    }
    let revision = rsdp[15];
    if revision >= 2 && rsdp.len() >= 36 {
        let length = rd_u32(rsdp, 20) as usize;
        if (20..=rsdp.len()).contains(&length) && checksum_ok(&rsdp[0..length]) {
            let xsdt = rd_u64(rsdp, 24);
            if xsdt != 0 {
                return Some(RsdpInfo {
                    revision,
                    sdt_phys: PhysAddr(xsdt),
                    use_xsdt: true,
                });
            }
        }
    }
    Some(RsdpInfo {
        revision,
        sdt_phys: PhysAddr(rd_u32(rsdp, 16) as u64),
        use_xsdt: false,
    })
}

/// Iterate the physical addresses of the tables an RSDT/XSDT points at.
fn sdt_pointers(sdt: &[u8], use_xsdt: bool) -> impl Iterator<Item = PhysAddr> + '_ {
    let stride = if use_xsdt { 8 } else { 4 };
    let len = if sdt.len() >= 36 {
        sdt_length(sdt).min(sdt.len())
    } else {
        0
    };
    let count = len.saturating_sub(36) / stride;
    (0..count).map(move |i| {
        let off = 36 + i * stride;
        if use_xsdt {
            PhysAddr(rd_u64(sdt, off))
        } else {
            PhysAddr(rd_u32(sdt, off) as u64)
        }
    })
}

/// Whether a processor entry's CPU is usable, from its flags (ACPI 6.3 §5.2.12.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CpuState {
    /// Bit 0: the processor is ready for use.
    Enabled,
    /// Bit 1 with bit 0 clear: not present at boot, but the platform can bring it online.
    OnlineCapable,
    /// Neither: the entry describes a processor the OS must not use.
    Disabled,
}

impl CpuState {
    fn from_flags(flags: u32) -> CpuState {
        if flags & 1 != 0 {
            CpuState::Enabled
        } else if flags & 2 != 0 {
            CpuState::OnlineCapable
        } else {
            CpuState::Disabled
        }
    }

    fn name(self) -> &'static str {
        match self {
            CpuState::Enabled => "enabled",
            CpuState::OnlineCapable => "online-capable",
            CpuState::Disabled => "disabled",
        }
    }
}

/// One MADT interrupt-controller structure, decoded as far as a report needs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MadtEntry {
    /// Type 0 (Processor Local APIC, 8-bit ids) or type 9 (Processor Local x2APIC, 32-bit).
    /// Firmware may describe the same machine either way — type 9 is required for an APIC id
    /// above 254 and permitted below — so both are CPUs.
    Cpu { x2apic: bool, uid: u32, apic_id: u32, state: CpuState },
    /// Type 1.
    IoApic(IoApic),
    /// Type 2.
    Override(SourceOverride),
    /// Type 4 (Local APIC NMI, 8-bit uid; `0xFF` means every CPU) or type 0xA (Local x2APIC
    /// NMI, 32-bit uid; `0xFFFF_FFFF` means every CPU).
    Nmi { x2apic: bool, uid: u32, lint: u8, flags: u16 },
    /// Any other type, or a known type too short to decode.
    Other { etype: u8, len: u8 },
}

/// The MADT's header fields after the SDT header: the local APIC address and the flags
/// (bit 0: dual 8259s are installed). `None` for a table too short to have them.
fn madt_header(madt: &[u8]) -> Option<(u32, u32)> {
    (madt.len() >= 44).then(|| (rd_u32(madt, 36), rd_u32(madt, 40)))
}

/// Iterate a MADT's entries in table order. Stops at the first entry whose length is shorter
/// than its own two-byte header or runs past the table, since nothing after it can be located.
fn madt_entries(madt: &[u8]) -> impl Iterator<Item = MadtEntry> + '_ {
    // Entries begin after the 36-byte SDT header + 8 bytes (local-APIC address u32 + flags u32).
    let mut off = 44;
    core::iter::from_fn(move || {
        if off + 2 > madt.len() {
            return None;
        }
        let etype = madt[off];
        let elen = madt[off + 1] as usize;
        if elen < 2 || off + elen > madt.len() {
            return None;
        }
        let e = &madt[off..off + elen];
        off += elen;
        Some(match etype {
            0 if elen >= 8 => MadtEntry::Cpu {
                x2apic: false,
                uid: e[2] as u32,
                apic_id: e[3] as u32,
                state: CpuState::from_flags(rd_u32(e, 4)),
            },
            1 if elen >= 12 => MadtEntry::IoApic(IoApic {
                id: e[2],
                addr: PhysAddr(rd_u32(e, 4) as u64),
                gsi_base: rd_u32(e, 8),
            }),
            2 if elen >= 10 => MadtEntry::Override(SourceOverride {
                source_irq: e[3],
                gsi: rd_u32(e, 4),
                flags: rd_u16(e, 8),
            }),
            4 if elen >= 6 => {
                MadtEntry::Nmi { x2apic: false, uid: e[2] as u32, flags: rd_u16(e, 3), lint: e[5] }
            }
            9 if elen >= 16 => MadtEntry::Cpu {
                x2apic: true,
                apic_id: rd_u32(e, 4),
                state: CpuState::from_flags(rd_u32(e, 8)),
                uid: rd_u32(e, 12),
            },
            0xA if elen >= 12 => {
                MadtEntry::Nmi { x2apic: true, flags: rd_u16(e, 2), uid: rd_u32(e, 4), lint: e[8] }
            }
            _ => MadtEntry::Other { etype, len: elen as u8 },
        })
    })
}

/// Parse a MADT ("APIC" table) into the caller's buffers, truncating at each buffer's
/// capacity. Returns `(n_ioapic, n_override, n_cpu)`, where a CPU is an **enabled** entry of
/// either processor type — online-capable ones are not present at boot.
fn parse_madt(
    madt: &[u8],
    ioapics: &mut [IoApic],
    overrides: &mut [SourceOverride],
    cpus: &mut [u32],
) -> (usize, usize, usize) {
    let (mut ni, mut no, mut nc) = (0, 0, 0);
    for entry in madt_entries(madt) {
        match entry {
            MadtEntry::Cpu { apic_id, state: CpuState::Enabled, .. } if nc < cpus.len() => {
                cpus[nc] = apic_id;
                nc += 1;
            }
            MadtEntry::IoApic(io) if ni < ioapics.len() => {
                ioapics[ni] = io;
                ni += 1;
            }
            MadtEntry::Override(ov) if no < overrides.len() => {
                overrides[no] = ov;
                no += 1;
            }
            _ => {}
        }
    }
    (ni, no, nc)
}

/// Log one MADT entry on one line.
fn log_madt_entry(entry: &MadtEntry) {
    match *entry {
        MadtEntry::Cpu { x2apic, uid, apic_id, state } => crate::kprintln!(
            "madt: {} uid {} apic {} {}",
            if x2apic { "x2apic" } else { "lapic" },
            uid,
            apic_id,
            state.name()
        ),
        MadtEntry::IoApic(io) => crate::kprintln!(
            "madt: ioapic {} @{:#x} gsi {}",
            io.id,
            io.addr.as_u64(),
            io.gsi_base
        ),
        MadtEntry::Override(ov) => crate::kprintln!(
            "madt: override irq {} -> gsi {} flags {:#x}",
            ov.source_irq,
            ov.gsi,
            ov.flags
        ),
        MadtEntry::Nmi { x2apic, uid, lint, flags } => crate::kprintln!(
            "madt: {} uid {:#x} lint {} flags {:#x}",
            if x2apic { "x2apic-nmi" } else { "lapic-nmi" },
            uid,
            lint,
            flags
        ),
        MadtEntry::Other { etype, len } => {
            crate::kprintln!("madt: type {:#x} len {}", etype, len)
        }
    }
}

/// Parse an MCFG into the caller's buffer, truncating at its capacity. Returns
/// the number of ECAM regions written.
fn parse_mcfg(mcfg: &[u8], out: &mut [EcamRegion]) -> usize {
    let mut n = 0;
    // Entries begin after the 36-byte SDT header + 8 reserved bytes.
    let mut off = 44;
    while off + 16 <= mcfg.len() && n < out.len() {
        let e = &mcfg[off..off + 16];
        out[n] = EcamRegion {
            base: PhysAddr(rd_u64(e, 0)),
            segment: rd_u16(e, 8),
            bus_start: e[10],
            bus_end: e[11],
        };
        n += 1;
        off += 16;
    }
    n
}

/// Map a firmware physical address into a readable byte slice through the HHDM.
///
/// # Safety
/// `phys` must point at firmware-owned physical memory that Limine maps in the
/// HHDM (ACPI tables live in ACPI-reclaimable memory, which it does); `len`
/// bytes from there must be within that mapping. Callers cap `len`.
unsafe fn phys_slice(phys: PhysAddr, len: usize) -> &'static [u8] {
    let va = phys.as_u64() + heap::hhdm_offset();
    // SAFETY: forwarded from this function's contract; the HHDM maps the
    // firmware physical range and the bytes are read-only ACPI data.
    unsafe { core::slice::from_raw_parts(va as *const u8, len) }
}

/// The x86_64 [`ArchPlatform`] implementation (ACPI). Zero-sized; re-exported as
/// `crate::arch::Platform`.
pub struct X86Platform;

impl ArchPlatform for X86Platform {
    unsafe fn init() -> Result<(), AllocError> {
        // 1. Obtain the RSDP address from Limine.
        // SAFETY: the request static is written once by Limine before `_start`;
        // single-threaded boot read.
        let resp = unsafe { (&raw const RSDP_REQUEST).read().response };
        if resp.is_null() {
            crate::kprintln!("acpi: bootloader provided no RSDP — skipping table parse");
            return Ok(());
        }
        // SAFETY: non-null response written by Limine.
        let address = unsafe { (*resp).address };

        // Translate to a kernel-virtual pointer via the HHDM. Recent Limine
        // hands back a physical address; tolerate an already-virtual pointer
        // from older bootloaders (a physical RSDP is far below the HHDM base).
        let hhdm = heap::hhdm_offset();
        let va = if address >= hhdm { address } else { address + hhdm };
        // SAFETY: the RSDP lives in firmware memory mapped by the HHDM; reading
        // up to 36 bytes (the max RSDP size) is within the mapping.
        let rsdp = unsafe { core::slice::from_raw_parts(va as *const u8, 36) };

        let info = match parse_rsdp(rsdp) {
            Some(i) => i,
            None => {
                crate::kprintln!("acpi: RSDP signature/checksum invalid — skipping");
                return Ok(());
            }
        };

        // 2. Map the root table (XSDT/RSDT): read its header for the length,
        // then re-map the whole (capped) table.
        // SAFETY: `sdt_phys` came from the validated RSDP; 36 header bytes.
        let sdt_hdr = unsafe { phys_slice(info.sdt_phys, 36) };
        let sdt_len = sdt_length(sdt_hdr).min(MAX_TABLE_BYTES);
        // SAFETY: as above, for the capped full length.
        let sdt = unsafe { phys_slice(info.sdt_phys, sdt_len) };
        crate::kprintln!(
            "acpi: RSDP rev {} oem {}; {} @{:#x}",
            info.revision,
            Printable(&rsdp[9..15]),
            if info.use_xsdt { "XSDT" } else { "RSDT" },
            info.sdt_phys.as_u64(),
        );

        // 3. Walk the SDT pointers: one line per table, then parse the first MADT and first
        // MCFG into local buffers. The MADT's entries are logged after the table list, so the
        // list reads as one block on the screen.
        let mut ioapics = [IoApic::ZERO; MAX_IOAPIC];
        let mut overrides = [SourceOverride::ZERO; MAX_OVERRIDE];
        let mut cpus = [0u32; MAX_CPU];
        let mut ecam = [EcamRegion::ZERO; MAX_ECAM];
        let (mut ni, mut no, mut nc, mut ne) = (0usize, 0usize, 0usize, 0usize);
        let mut madt: Option<&[u8]> = None;
        let mut have_mcfg = false;

        for tphys in sdt_pointers(sdt, info.use_xsdt) {
            // SAFETY: each pointer is from the validated root table; read 36
            // header bytes first to learn the table's signature and length.
            let hdr = unsafe { phys_slice(tphys, 36) };
            let sig = match sdt_signature(hdr) {
                Some(s) => s,
                None => continue,
            };
            let tlen = sdt_length(hdr).min(MAX_TABLE_BYTES);
            // SAFETY: as above, for the capped full length.
            let table = unsafe { phys_slice(tphys, tlen) };
            let valid = checksum_ok(table);
            crate::kprintln!(
                "acpi: {} @{:#x} rev {} len {} oem {} {}{}",
                Printable(&sig),
                tphys.as_u64(),
                hdr[8],
                sdt_length(hdr),
                Printable(&hdr[10..16]),
                Printable(&hdr[16..24]),
                if valid { "" } else { " — bad checksum, ignored" },
            );
            if !valid {
                continue;
            }
            match &sig {
                b"APIC" if madt.is_none() => {
                    let (a, b, c) = parse_madt(table, &mut ioapics, &mut overrides, &mut cpus);
                    ni = a;
                    no = b;
                    nc = c;
                    madt = Some(table);
                }
                b"MCFG" if !have_mcfg => {
                    ne = parse_mcfg(table, &mut ecam);
                    have_mcfg = true;
                }
                _ => {}
            }
        }

        // 4. Commit to the static caches (sole writer; boot, pre-interrupts).
        // SAFETY: single-threaded boot before interrupts/SMP; nothing else
        // touches these statics, and no reader runs until `init` returns.
        unsafe {
            (&raw mut IOAPICS).write(ioapics);
            (&raw mut OVERRIDES).write(overrides);
            (&raw mut CPU_APIC_IDS).write(cpus);
            (&raw mut ECAM).write(ecam);
        }
        IOAPIC_COUNT.store(ni, Ordering::Release);
        OVERRIDE_COUNT.store(no, Ordering::Release);
        CPU_COUNT.store(nc, Ordering::Release);
        ECAM_COUNT.store(ne, Ordering::Release);

        // 5. The MADT entry by entry, then the summary counted from the same walk, then every
        // ECAM window.
        if let Some(madt) = madt {
            if let Some((lapic, flags)) = madt_header(madt) {
                crate::kprintln!("madt: local APIC @{:#x} flags {:#x}", lapic, flags);
            }
            for entry in madt_entries(madt) {
                log_madt_entry(&entry);
            }
        }
        crate::kprintln!(
            "acpi: {} IOAPIC, {} src-override, {} CPU; {} ECAM region",
            ni,
            no,
            nc,
            ne,
        );
        for e in &ecam[..ne] {
            crate::kprintln!(
                "acpi: ECAM @{:#x} seg {} bus {}-{}",
                e.base.as_u64(),
                e.segment,
                e.bus_start,
                e.bus_end
            );
        }
        Ok(())
    }

    fn pcie_ecam_regions() -> &'static [EcamRegion] {
        let n = ECAM_COUNT.load(Ordering::Acquire);
        // SAFETY: `ECAM` is written once by `init` before any reader runs and is
        // read-only thereafter; `n` entries are initialised.
        unsafe { core::slice::from_raw_parts((&raw const ECAM) as *const EcamRegion, n) }
    }
}

// The arch-internal MADT accessors below are the seam the `phase-2/ioapic`
// item consumes; they are parsed and cached now but have no in-tree caller yet.
/// The I/O APICs discovered from the MADT (arch-internal; the x86 IOAPIC bring-up
/// consumes this — it does **not** cross the arch boundary).
#[allow(dead_code)]
pub(crate) fn ioapics() -> &'static [IoApic] {
    let n = IOAPIC_COUNT.load(Ordering::Acquire);
    // SAFETY: as `pcie_ecam_regions`.
    unsafe { core::slice::from_raw_parts((&raw const IOAPICS) as *const IoApic, n) }
}

/// The ISA-IRQ → GSI source overrides from the MADT (arch-internal).
#[allow(dead_code)]
pub(crate) fn source_overrides() -> &'static [SourceOverride] {
    let n = OVERRIDE_COUNT.load(Ordering::Acquire);
    // SAFETY: as `pcie_ecam_regions`.
    unsafe { core::slice::from_raw_parts((&raw const OVERRIDES) as *const SourceOverride, n) }
}

/// The enabled CPUs' APIC ids from the MADT, from type-0 and type-9 entries alike
/// (arch-internal; SMP bring-up).
#[allow(dead_code)]
pub(crate) fn cpu_apic_ids() -> &'static [u32] {
    let n = CPU_COUNT.load(Ordering::Acquire);
    // SAFETY: as `pcie_ecam_regions`.
    unsafe { core::slice::from_raw_parts((&raw const CPU_APIC_IDS) as *const u32, n) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set the checksum byte (at `csum_off`) so the table's bytes sum to zero.
    fn fix_checksum(table: &mut [u8], csum_off: usize) {
        table[csum_off] = 0;
        let sum = table.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        table[csum_off] = 0u8.wrapping_sub(sum);
    }

    /// Build a minimal valid SDT header with the given signature and length.
    fn sdt_header(sig: &[u8; 4], length: usize) -> Vec<u8> {
        let mut h = vec![0u8; 36];
        h[0..4].copy_from_slice(sig);
        h[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        h
    }

    fn v2_rsdp(xsdt_addr: u64) -> Vec<u8> {
        let mut r = vec![0u8; 36];
        r[0..8].copy_from_slice(b"RSD PTR ");
        r[9..15].copy_from_slice(b"NITROX");
        r[15] = 2; // revision (ACPI 2.0+)
        r[16..20].copy_from_slice(&0u32.to_le_bytes()); // rsdt addr (unused)
        r[20..24].copy_from_slice(&36u32.to_le_bytes()); // length
        r[24..32].copy_from_slice(&xsdt_addr.to_le_bytes());
        fix_checksum(&mut r[0..20], 8); // v1 checksum over first 20 bytes
        // extended checksum over the full 36 bytes (byte 32)
        r[32] = 0;
        let sum = r.iter().fold(0u8, |a, &x| a.wrapping_add(x));
        r[32] = 0u8.wrapping_sub(sum);
        r
    }

    #[test]
    fn parse_rsdp_v2_prefers_xsdt() {
        let r = v2_rsdp(0xDEAD_BEEF_0000);
        let info = parse_rsdp(&r).expect("valid RSDP");
        assert!(info.use_xsdt);
        assert_eq!(info.sdt_phys, PhysAddr(0xDEAD_BEEF_0000));
        assert_eq!(info.revision, 2);
    }

    #[test]
    fn parse_rsdp_v1_uses_rsdt() {
        let mut r = vec![0u8; 20];
        r[0..8].copy_from_slice(b"RSD PTR ");
        r[15] = 0; // ACPI 1.0
        r[16..20].copy_from_slice(&0x000F_0000u32.to_le_bytes());
        fix_checksum(&mut r, 8);
        let info = parse_rsdp(&r).expect("valid v1 RSDP");
        assert!(!info.use_xsdt);
        assert_eq!(info.sdt_phys, PhysAddr(0x000F_0000));
    }

    #[test]
    fn parse_rsdp_rejects_bad_signature_and_checksum() {
        let mut bad_sig = v2_rsdp(0x1000);
        bad_sig[0] = b'X';
        assert_eq!(parse_rsdp(&bad_sig), None);

        let mut bad_csum = v2_rsdp(0x1000);
        bad_csum[8] = bad_csum[8].wrapping_add(1); // break the v1 checksum
        assert_eq!(parse_rsdp(&bad_csum), None);
    }

    #[test]
    fn sdt_pointers_walks_xsdt_and_rsdt() {
        // XSDT with two 64-bit pointers.
        let mut xsdt = sdt_header(b"XSDT", 36 + 16);
        xsdt.extend_from_slice(&0x1111u64.to_le_bytes());
        xsdt.extend_from_slice(&0x2222u64.to_le_bytes());
        let got: Vec<PhysAddr> = sdt_pointers(&xsdt, true).collect();
        assert_eq!(got, vec![PhysAddr(0x1111), PhysAddr(0x2222)]);

        // RSDT with two 32-bit pointers.
        let mut rsdt = sdt_header(b"RSDT", 36 + 8);
        rsdt.extend_from_slice(&0x3333u32.to_le_bytes());
        rsdt.extend_from_slice(&0x4444u32.to_le_bytes());
        let got: Vec<PhysAddr> = sdt_pointers(&rsdt, false).collect();
        assert_eq!(got, vec![PhysAddr(0x3333), PhysAddr(0x4444)]);
    }

    #[test]
    fn parse_madt_extracts_ioapic_override_and_cpu() {
        let mut madt = sdt_header(b"APIC", 0);
        madt.extend_from_slice(&0xFEE0_0000u32.to_le_bytes()); // local APIC addr
        madt.extend_from_slice(&1u32.to_le_bytes()); // flags
        // type 0 Processor Local APIC, enabled
        madt.extend_from_slice(&[0, 8, 0, 5]);
        madt.extend_from_slice(&1u32.to_le_bytes());
        // type 0 Processor Local APIC, disabled (flags=0) — must be skipped
        madt.extend_from_slice(&[0, 8, 1, 6]);
        madt.extend_from_slice(&0u32.to_le_bytes());
        // type 1 I/O APIC: id 2, addr 0xFEC00000, gsi_base 0
        madt.extend_from_slice(&[1, 12, 2, 0]);
        madt.extend_from_slice(&0xFEC0_0000u32.to_le_bytes());
        madt.extend_from_slice(&0u32.to_le_bytes());
        // type 2 Interrupt Source Override: ISA IRQ 0 -> GSI 2, flags 0
        madt.extend_from_slice(&[2, 10, 0, 0]);
        madt.extend_from_slice(&2u32.to_le_bytes());
        madt.extend_from_slice(&0u16.to_le_bytes());

        let mut ioapics = [IoApic::ZERO; 4];
        let mut overrides = [SourceOverride::ZERO; 4];
        let mut cpus = [0u32; 4];
        let (ni, no, nc) = parse_madt(&madt, &mut ioapics, &mut overrides, &mut cpus);
        assert_eq!((ni, no, nc), (1, 1, 1));
        assert_eq!(
            ioapics[0],
            IoApic { id: 2, addr: PhysAddr(0xFEC0_0000), gsi_base: 0 }
        );
        assert_eq!(overrides[0], SourceOverride { source_irq: 0, gsi: 2, flags: 0 });
        assert_eq!(cpus[0], 5); // only the enabled CPU
    }

    /// QEMU q35's MADT at `-smp 4`, as the kernel read it (captured 2026-09-14 by a
    /// temporary hex dump in `X86Platform::init` under `cargo xtask test-qemu`, then removed).
    /// Checksum-valid as captured.
    const QEMU_MADT: [u8; 144] = [
        0x41, 0x50, 0x49, 0x43, 0x90, 0x00, 0x00, 0x00, 0x03, 0x49, 0x42, 0x4f,
        0x43, 0x48, 0x53, 0x20, 0x42, 0x58, 0x50, 0x43, 0x20, 0x20, 0x20, 0x20,
        0x01, 0x00, 0x00, 0x00, 0x42, 0x58, 0x50, 0x43, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xe0, 0xfe, 0x01, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x08, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x08, 0x02, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x08, 0x03, 0x03,
        0x01, 0x00, 0x00, 0x00, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x00, 0xc0, 0xfe,
        0x00, 0x00, 0x00, 0x00, 0x02, 0x0a, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x02, 0x0a, 0x00, 0x05, 0x05, 0x00, 0x00, 0x00, 0x0d, 0x00,
        0x02, 0x0a, 0x00, 0x09, 0x09, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x02, 0x0a,
        0x00, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x02, 0x0a, 0x00, 0x0b,
        0x0b, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x04, 0x06, 0xff, 0x00, 0x00, 0x01,
    ];

    #[test]
    fn qemus_captured_madt_decodes_entry_by_entry() {
        assert!(checksum_ok(&QEMU_MADT), "the capture is a whole, valid table");
        assert_eq!(madt_header(&QEMU_MADT), Some((0xFEE0_0000, 1)));
        let cpu = |n: u32| MadtEntry::Cpu { x2apic: false, uid: n, apic_id: n, state: CpuState::Enabled };
        let level_high = |irq: u8| {
            MadtEntry::Override(SourceOverride { source_irq: irq, gsi: irq as u32, flags: 0xD })
        };
        let got: Vec<MadtEntry> = madt_entries(&QEMU_MADT).collect();
        assert_eq!(
            got,
            vec![
                cpu(0),
                cpu(1),
                cpu(2),
                cpu(3),
                MadtEntry::IoApic(IoApic { id: 0, addr: PhysAddr(0xFEC0_0000), gsi_base: 0 }),
                MadtEntry::Override(SourceOverride { source_irq: 0, gsi: 2, flags: 0 }),
                level_high(5),
                level_high(9),
                level_high(10),
                level_high(11),
                MadtEntry::Nmi { x2apic: false, uid: 0xFF, lint: 1, flags: 0 },
            ]
        );
        let mut io = [IoApic::ZERO; 8];
        let mut ov = [SourceOverride::ZERO; 8];
        let mut cp = [0u32; 8];
        assert_eq!(parse_madt(&QEMU_MADT, &mut io, &mut ov, &mut cp), (1, 5, 4));
        assert_eq!(&cp[..4], &[0, 1, 2, 3]);
    }

    #[test]
    fn x2apic_cpus_count_and_online_capable_ones_are_listed_but_not_counted() {
        // Firmware that describes its CPUs only as type-9 entries: the parser before Part D
        // matched type 0 alone and would have counted none of these.
        let mut madt = sdt_header(b"APIC", 0);
        madt.extend_from_slice(&0xFEE0_0000u32.to_le_bytes());
        madt.extend_from_slice(&0u32.to_le_bytes());
        let x2apic = |madt: &mut Vec<u8>, apic: u32, flags: u32, uid: u32| {
            madt.extend_from_slice(&[9, 16, 0, 0]);
            madt.extend_from_slice(&apic.to_le_bytes());
            madt.extend_from_slice(&flags.to_le_bytes());
            madt.extend_from_slice(&uid.to_le_bytes());
        };
        x2apic(&mut madt, 0x100, 1, 7); // enabled, an id no type-0 entry can hold
        x2apic(&mut madt, 0x102, 2, 8); // online-capable: not present at boot
        x2apic(&mut madt, 0x104, 0, 9); // disabled
        // type 0 online-capable
        madt.extend_from_slice(&[0, 8, 3, 3]);
        madt.extend_from_slice(&2u32.to_le_bytes());
        // type 0xA Local x2APIC NMI: flags 0x5, every CPU, LINT1
        madt.extend_from_slice(&[0xA, 12]);
        madt.extend_from_slice(&5u16.to_le_bytes());
        madt.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        madt.extend_from_slice(&[1, 0, 0, 0]);
        // an entry type this parser has no name for
        madt.extend_from_slice(&[0x7F, 4, 0, 0]);

        let got: Vec<MadtEntry> = madt_entries(&madt).collect();
        assert_eq!(
            got,
            vec![
                MadtEntry::Cpu { x2apic: true, uid: 7, apic_id: 0x100, state: CpuState::Enabled },
                MadtEntry::Cpu { x2apic: true, uid: 8, apic_id: 0x102, state: CpuState::OnlineCapable },
                MadtEntry::Cpu { x2apic: true, uid: 9, apic_id: 0x104, state: CpuState::Disabled },
                MadtEntry::Cpu { x2apic: false, uid: 3, apic_id: 3, state: CpuState::OnlineCapable },
                MadtEntry::Nmi { x2apic: true, uid: 0xFFFF_FFFF, lint: 1, flags: 5 },
                MadtEntry::Other { etype: 0x7F, len: 4 },
            ]
        );
        let mut io = [IoApic::ZERO; 2];
        let mut ov = [SourceOverride::ZERO; 2];
        let mut cp = [0u32; 4];
        assert_eq!(parse_madt(&madt, &mut io, &mut ov, &mut cp), (0, 0, 1));
        assert_eq!(cp[0], 0x100);
    }

    #[test]
    fn a_known_entry_type_too_short_to_decode_is_other_and_a_bad_length_ends_the_walk() {
        let mut madt = sdt_header(b"APIC", 0);
        madt.extend_from_slice(&[0u8; 8]);
        // A type-9 entry claiming 8 bytes: too short for its fields, but locatable.
        madt.extend_from_slice(&[9, 8, 0, 0, 1, 0, 0, 0]);
        // A length of 1 cannot even cover its own header; nothing after it is reachable.
        madt.extend_from_slice(&[0, 1]);
        madt.extend_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
        let got: Vec<MadtEntry> = madt_entries(&madt).collect();
        assert_eq!(got, vec![MadtEntry::Other { etype: 9, len: 8 }]);
    }

    #[test]
    fn parse_madt_truncates_at_buffer_capacity() {
        let mut madt = sdt_header(b"APIC", 0);
        madt.extend_from_slice(&0u32.to_le_bytes());
        madt.extend_from_slice(&0u32.to_le_bytes());
        for id in 0..4u8 {
            madt.extend_from_slice(&[1, 12, id, 0]);
            madt.extend_from_slice(&0xFEC0_0000u32.to_le_bytes());
            madt.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut ioapics = [IoApic::ZERO; 2]; // capacity 2 < 4 entries
        let mut overrides = [SourceOverride::ZERO; 4];
        let mut cpus = [0u32; 4];
        let (ni, _, _) = parse_madt(&madt, &mut ioapics, &mut overrides, &mut cpus);
        assert_eq!(ni, 2, "must truncate at buffer capacity, not overrun");
    }

    #[test]
    fn parse_mcfg_extracts_ecam_region() {
        let mut mcfg = sdt_header(b"MCFG", 0);
        mcfg.extend_from_slice(&0u64.to_le_bytes()); // reserved
        // one allocation: base 0xB000_0000, seg 0, bus 0..=255
        mcfg.extend_from_slice(&0xB000_0000u64.to_le_bytes());
        mcfg.extend_from_slice(&0u16.to_le_bytes());
        mcfg.push(0); // bus_start
        mcfg.push(255); // bus_end
        mcfg.extend_from_slice(&0u32.to_le_bytes()); // reserved

        let mut ecam = [EcamRegion::ZERO; 4];
        let n = parse_mcfg(&mcfg, &mut ecam);
        assert_eq!(n, 1);
        assert_eq!(
            ecam[0],
            EcamRegion { base: PhysAddr(0xB000_0000), segment: 0, bus_start: 0, bus_end: 255 }
        );
    }

    #[test]
    fn short_tables_do_not_panic() {
        assert_eq!(parse_rsdp(&[]), None);
        assert_eq!(parse_rsdp(&[0u8; 8]), None);
        assert_eq!(sdt_signature(&[0u8; 10]), None);
        assert_eq!(sdt_pointers(&[0u8; 10], true).count(), 0);
        let mut io = [IoApic::ZERO; 2];
        let mut ov = [SourceOverride::ZERO; 2];
        let mut cp = [0u32; 2];
        assert_eq!(parse_madt(&[0u8; 10], &mut io, &mut ov, &mut cp), (0, 0, 0));
        assert_eq!(parse_mcfg(&[0u8; 10], &mut [EcamRegion::ZERO; 2]), 0);
    }
}
