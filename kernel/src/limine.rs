//! Limine boot protocol bindings (v12, base revision 6).
//!
//! Hand-rolled `#[repr(C)]` mirrors of the protocol headers from
//! <https://github.com/limine-bootloader/limine-protocol>. The kernel/CLAUDE.md
//! rules forbid external crates, so we do not use the `limine` crate; the
//! ID/struct values below are pinned to the trunk of `limine-protocol` as of
//! Limine 12.2.0.
//!
//! Each request is a `#[repr(C)]` struct whose first four `u64`s are the
//! protocol-assigned ID, followed by a `revision` field and a response
//! pointer that the bootloader populates before jumping to the kernel.
//!
//! Request statics live in the `.limine_requests` ELF section (see
//! `kernel/linker.ld`). Limine scans this region — the explicit start/end
//! markers `RequestsStartMarker`/`RequestsEndMarker` speed up that scan and
//! are mandatory under base revision 6.

#![allow(dead_code, reason = "the protocol's structures are mirrored whole; the kernel reads some of their fields")]
#![allow(clippy::new_without_default, reason = "each request is built once, as a static, with its protocol ID")]

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64};

/// First two ID `u64`s shared by every Limine request.
pub const COMMON_MAGIC_0: u64 = 0xc7b1dd30df4c8b88;
pub const COMMON_MAGIC_1: u64 = 0x0a82e883a194f07b;

/// Base revision marker. The bootloader zeroes `revision` if it supports
/// the requested protocol revision. We must check it before trusting any
/// response.
///
/// Layout: two magic `u64`s followed by a mutable revision slot.
#[repr(C)]
pub struct BaseRevision {
    pub magic_0: u64,
    pub magic_1: u64,
    pub revision: u64,
}

impl BaseRevision {
    pub const fn new(revision: u64) -> Self {
        Self { magic_0: 0xf9562b2d5c95a6c8, magic_1: 0x6a7b384944536bdc, revision }
    }

    /// `true` if the bootloader honoured our requested revision.
    pub fn supported(&self) -> bool {
        // SAFETY: `self.revision` is a plain `u64`; volatile read avoids the
        // optimiser caching the original value from before Limine zeroed it.
        unsafe { ptr::read_volatile(&self.revision) == 0 }
    }

    /// The base revision the bootloader says it loaded the kernel under, or `None` from a
    /// bootloader too old to say (`LIMINE_LOADED_BASE_REVISION`: it overwrites the second
    /// magic, which is therefore only a revision once it no longer reads as the magic).
    pub fn loaded(&self) -> Option<u64> {
        // SAFETY: as `supported` — a plain `u64` Limine may have written before `_start`.
        let v = unsafe { ptr::read_volatile(&self.magic_1) };
        (v != 0x6a7b384944536bdc).then_some(v)
    }
}

/// Start-of-requests marker (4 × u64). Lives in `.limine_requests_start`.
#[repr(C)]
pub struct RequestsStartMarker(pub [u64; 4]);

impl RequestsStartMarker {
    pub const fn new() -> Self {
        Self([0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9])
    }
}

/// End-of-requests marker (2 × u64). Lives in `.limine_requests_end`.
#[repr(C)]
pub struct RequestsEndMarker(pub [u64; 2]);

impl RequestsEndMarker {
    pub const fn new() -> Self {
        Self([0xadc0e0531bb10d03, 0x9572709f31764c62])
    }
}

/// **A Limine request**, carrying `X` after the response Limine writes before `_start`.
#[repr(C)]
pub struct Request<R, X = ()> {
    id: [u64; 4],
    revision: u64,
    response: AtomicPtr<R>,
    extra: X,
}

impl<R, X> Request<R, X> {
    const fn with(id_2: u64, id_3: u64, extra: X) -> Self {
        Request {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, id_2, id_3],
            revision: 0,
            response: AtomicPtr::new(ptr::null_mut()),
            extra,
        }
    }

    /// **The response Limine wrote**, if any: read volatile, so nothing folds it to its first null.
    pub fn response(&self) -> Option<&R> {
        // SAFETY: this request's own pointer field, aligned, which nothing in the program writes.
        let p = unsafe { ptr::read_volatile(self.response.as_ptr()) };
        // SAFETY: Limine writes null or a valid `R` before `_start`, in memory never reclaimed.
        unsafe { p.as_ref() }
    }
}

/// **The `count` entries of a Limine array of pointers**, those that are not null.
///
/// # Safety
/// `array` points to `count` pointers, each null or to a valid `T`, all as long-lived as `'a`.
unsafe fn entries<'a, T: 'a>(array: *const *mut T, count: u64) -> impl Iterator<Item = &'a T> {
    // SAFETY: `i < count`, and each pointer is null or valid, by the contract.
    (0..count as usize).filter_map(move |i| unsafe { (*array.add(i)).as_ref() })
}

/// A Limine string, cut at `max` bytes; empty where there is none.
///
/// # Safety
/// `p` is null or a NUL-terminated string as long-lived as `'a`.
unsafe fn string<'a>(p: *const u8, max: usize) -> &'a [u8] {
    // SAFETY: by the contract; `c_bytes` stops at `max`.
    unsafe { crate::libkern::printable::c_bytes(p, max) }
}

// --- Framebuffer request -------------------------------------------------

const FB_ID_2: u64 = 0x9d5827dcd881dd75;
const FB_ID_3: u64 = 0xa3148604f6fab11b;

/// `memory_model` value for ordinary packed RGB framebuffers (the only kind
/// Limine emits today). Other values are reserved for future use.
pub const FB_MODEL_RGB: u8 = 1;

pub type FramebufferRequest = Request<FramebufferResponse>;

impl FramebufferRequest {
    pub const fn new() -> Self {
        Request::with(FB_ID_2, FB_ID_3, ())
    }
}

#[repr(C)]
pub struct FramebufferResponse {
    revision: u64,
    framebuffer_count: u64,
    framebuffers: *mut *mut Framebuffer,
}

impl FramebufferResponse {
    /// The first framebuffer, the one the kernel draws on, if Limine reported any.
    pub fn first(&self) -> Option<&Framebuffer> {
        // SAFETY: Limine's array of `framebuffer_count` framebuffers.
        unsafe { entries(self.framebuffers, self.framebuffer_count) }.next()
    }
}

/// Per-framebuffer descriptor. Layout matches `struct limine_framebuffer`
/// from `limine-protocol`'s `limine.h` at base revision 6.
#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
    pub _unused: [u8; 7],
    pub edid_size: u64,
    pub edid: *mut u8,
    // Response revision 1 — present in the layout even when unused by us.
    pub mode_count: u64,
    pub modes: *mut *mut u8,
}

// --- Memory map request --------------------------------------------------
//
// Limine populates a list of physical-memory ranges with type tags
// (Usable, Reserved, ACPI Reclaimable, etc.). The buddy allocator consumes
// only `Usable` entries; others are passed through untouched.

const MEMMAP_ID_2: u64 = 0x67cf3d9d378a806f;
const MEMMAP_ID_3: u64 = 0xe304acdfc50c3c62;

/// Usable RAM — free for the kernel to manage.
pub const MEMMAP_USABLE: u64 = 0;
/// Firmware-reserved; never claim.
pub const MEMMAP_RESERVED: u64 = 1;
/// ACPI tables that may be reclaimed once parsing is complete.
pub const MEMMAP_ACPI_RECLAIMABLE: u64 = 2;
/// ACPI non-volatile storage; never claim.
pub const MEMMAP_ACPI_NVS: u64 = 3;
/// Hardware flagged as defective; never claim.
pub const MEMMAP_BAD_MEMORY: u64 = 4;
/// Bootloader's working memory; reclaimable once we own the boot stack.
pub const MEMMAP_BOOTLOADER_RECLAIMABLE: u64 = 5;
/// Memory occupied by the kernel ELF and any loaded modules.
pub const MEMMAP_KERNEL_AND_MODULES: u64 = 6;
/// Linear framebuffer backing store.
pub const MEMMAP_FRAMEBUFFER: u64 = 7;
/// Reserved, but mapped in the HHDM anyway (`LIMINE_MEMMAP_RESERVED_MAPPED`).
pub const MEMMAP_RESERVED_MAPPED: u64 = 8;

pub type MemoryMapRequest = Request<MemoryMapResponse>;

impl MemoryMapRequest {
    pub const fn new() -> Self {
        Request::with(MEMMAP_ID_2, MEMMAP_ID_3, ())
    }
}

#[repr(C)]
pub struct MemoryMapResponse {
    revision: u64,
    entry_count: u64,
    entries: *mut *mut MemoryMapEntry,
}

impl MemoryMapResponse {
    /// Its entries, in Limine's order.
    pub fn entries(&self) -> impl Iterator<Item = &MemoryMapEntry> {
        // SAFETY: Limine's array of `entry_count` entries.
        unsafe { entries(self.entries, self.entry_count) }
    }

    /// A memory map over `count` entries at `entries`, for a host test.
    ///
    /// # Safety
    /// As Limine's: `entries` points to `count` pointers to entries that outlive the map.
    #[cfg(test)]
    pub unsafe fn fabricated(count: u64, entries: *mut *mut MemoryMapEntry) -> Self {
        MemoryMapResponse { revision: 0, entry_count: count, entries }
    }
}

#[repr(C)]
pub struct MemoryMapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

// --- Higher-Half Direct Map (HHDM) request -------------------------------
//
// Limine maps all of physical memory at a fixed offset in the higher half
// (typically 0xffff800000000000). The kernel reaches a physical address `p`
// by reading `(p + hhdm_offset) as *mut _`. The buddy allocator uses this
// to access the first 8 bytes of each free frame (its intrusive next
// pointer) and to zero the coalesce bitmap during init.

const HHDM_ID_2: u64 = 0x48dcf1cb8ad2b852;
const HHDM_ID_3: u64 = 0x63984e959a98244b;

pub type HhdmRequest = Request<HhdmResponse>;

impl HhdmRequest {
    pub const fn new() -> Self {
        Request::with(HHDM_ID_2, HHDM_ID_3, ())
    }
}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset: u64,
}

// --- Module request ------------------------------------------------------
//
// Limine loads each configured `module_path` into memory (tagged
// `MEMMAP_KERNEL_AND_MODULES`, mapped in the HHDM) and hands back an array of
// `LimineFile` descriptors. Nitrox loads exactly one module — the initramfs
// CPIO blob — which init reads via the in-kernel `/initramfs` resource server.

const MODULE_ID_2: u64 = 0x3e7e279702be32af;
const MODULE_ID_3: u64 = 0xca1c4f3bd1280cee;

pub type ModuleRequest = Request<ModuleResponse>;

impl ModuleRequest {
    pub const fn new() -> Self {
        Request::with(MODULE_ID_2, MODULE_ID_3, ())
    }
}

#[repr(C)]
pub struct ModuleResponse {
    revision: u64,
    module_count: u64,
    modules: *mut *mut LimineFile,
}

impl ModuleResponse {
    /// The modules, in the order the boot entry names them: the initramfs first.
    pub fn modules(&self) -> impl Iterator<Item = &LimineFile> {
        // SAFETY: Limine's array of `module_count` files.
        unsafe { entries(self.modules, self.module_count) }
    }
}

/// A loaded module descriptor (`struct limine_file`). `address` is an HHDM-virtual pointer,
/// directly dereferenceable.
///
/// **The media fields after `path` say where the file was loaded from**: for a
/// GPT volume, `gpt_disk_uuid` is its disk's GUID **in the byte order a GPT header stores it** —
/// measured on a live-stick boot and a disk boot against `sgdisk -p` — and the boot disk is the one
/// whose header holds the same bytes. Zero for a volume that is not GPT.
#[repr(C)]
pub struct LimineFile {
    pub revision: u64,
    pub address: *mut u8,
    pub size: u64,
    path: *const u8,
    /// The module's string (its `cmdline` before protocol 8); unread.
    string: *const u8,
    /// `0` generic, `1` optical, `2` TFTP.
    pub media_type: u32,
    pub unused: u32,
    pub tftp_ip: u32,
    pub tftp_port: u32,
    /// The partition it was on, from 1; `0` for a whole disk.
    pub partition_index: u32,
    pub mbr_disk_id: u32,
    pub gpt_disk_uuid: [u8; 16],
    pub gpt_part_uuid: [u8; 16],
    pub part_uuid: [u8; 16],
}

impl LimineFile {
    /// The path it was loaded from, cut at 4 KiB.
    pub fn path(&self) -> &[u8] {
        // SAFETY: Limine's string, NUL-terminated and never reclaimed.
        unsafe { string(self.path, 4096) }
    }
}

// --- SMP / MP (multiprocessor) request -----------------------------------
//
// Limine starts every application processor (AP) for us and parks each one
// spinning until the kernel writes its `goto_address`. An atomic write to that
// field makes the parked AP jump to the address — in 64-bit long mode, on a
// Limine-provided stack, with a `*const SmpInfo` (its own entry) in `RDI`. So
// there is no INIT/SIPI sequencing and no real-mode trampoline; an AP entry is
// an ordinary `extern "C"` function. We leave the request `flags` clear (do not
// ask Limine to enable x2APIC) so every CPU hands off in xAPIC mode and the
// kernel performs the x2APIC transition itself, uniformly (see `apic.rs`).

const SMP_ID_2: u64 = 0x95a67b819a1b857e;
const SMP_ID_3: u64 = 0xa0b61b723b6a73e0;

/// The SMP request, its `flags` after the response: 0, as the kernel enables x2APIC itself.
pub type SmpRequest = Request<SmpResponse, u64>;

impl SmpRequest {
    pub const fn new() -> Self {
        Request::with(SMP_ID_2, SMP_ID_3, 0)
    }
}

/// `struct limine_mp_response` (x86_64).
#[repr(C)]
pub struct SmpResponse {
    pub revision: u64,
    /// Bit 0 (`LIMINE_MP_RESPONSE_X86_64_X2APIC`): x2APIC was enabled by Limine.
    pub flags: u32,
    /// The boot processor's local-APIC id.
    pub bsp_lapic_id: u32,
    cpu_count: u64,
    cpus: *mut *mut SmpInfo,
}

impl SmpResponse {
    /// Every CPU, the boot processor's entry included.
    pub fn cpus(&self) -> impl Iterator<Item = &SmpInfo> {
        // SAFETY: Limine's array of `cpu_count` CPUs.
        unsafe { entries(self.cpus, self.cpu_count) }
    }
}

/// `struct limine_mp_info` (x86_64) — one per logical CPU.
#[repr(C)]
pub struct SmpInfo {
    /// The dense ACPI processor id (the logical CPU index Nitrox uses).
    pub processor_id: u32,
    /// The CPU's local-APIC id (its IPI destination).
    pub lapic_id: u32,
    pub reserved: u64,
    /// The kernel writes the AP's entry-point address here (an atomic write);
    /// the parked AP then jumps to it with this `SmpInfo*` in `RDI`. `0` parks.
    pub goto_address: AtomicU64,
    /// Free for the kernel; passed through untouched by Limine.
    pub extra_argument: u64,
}

// --- ACPI RSDP request ---------------------------------------------------
//
// Limine locates the ACPI Root System Description Pointer and hands back its
// address. On x86_64 the RSDP is the root of the ACPI table tree (RSDT/XSDT →
// MADT, MCFG, …); the platform layer parses it for interrupt routing and the
// PCIe ECAM window. Newer Limine revisions return a *physical* address (the
// consumer translates via the HHDM and tolerates an already-virtual pointer
// from older bootloaders). This is x86 firmware territory — aarch64 uses a
// Device Tree Blob request instead.

const RSDP_ID_2: u64 = 0xc5e77b6b397e7b43;
const RSDP_ID_3: u64 = 0x27637845accdcf3c;

pub type RsdpRequest = Request<RsdpResponse>;

impl RsdpRequest {
    pub const fn new() -> Self {
        Request::with(RSDP_ID_2, RSDP_ID_3, ())
    }
}

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    /// Address of the RSDP. Physical on recent Limine revisions (translate via
    /// the HHDM); may be an HHDM-virtual pointer on older bootloaders.
    pub address: u64,
}

// --- Bootloader info request ---------------------------------------------
//
// Which bootloader loaded us, and its version — the first line of a hardware report, since
// every other handoff value is that bootloader's reading of the machine.

const BOOTLOADER_INFO_ID_2: u64 = 0xf55038d8e2a1202f;
const BOOTLOADER_INFO_ID_3: u64 = 0x279426fcf5f59740;

pub type BootloaderInfoRequest = Request<BootloaderInfoResponse>;

impl BootloaderInfoRequest {
    pub const fn new() -> Self {
        Request::with(BOOTLOADER_INFO_ID_2, BOOTLOADER_INFO_ID_3, ())
    }
}

#[repr(C)]
pub struct BootloaderInfoResponse {
    revision: u64,
    name: *const u8,
    version: *const u8,
}

impl BootloaderInfoResponse {
    /// The bootloader's name, cut at 64 bytes.
    pub fn name(&self) -> &[u8] {
        // SAFETY: as `path`.
        unsafe { string(self.name, 64) }
    }

    /// Its version, cut at 64 bytes.
    pub fn version(&self) -> &[u8] {
        // SAFETY: as `path`.
        unsafe { string(self.version, 64) }
    }
}

// --- Firmware type request -----------------------------------------------

const FIRMWARE_TYPE_ID_2: u64 = 0x8c2f75d90bef28a8;
const FIRMWARE_TYPE_ID_3: u64 = 0x7045a4688eac00c3;

/// Booted by legacy x86 BIOS.
pub const FIRMWARE_X86_BIOS: u64 = 0;
/// Booted by 32-bit UEFI.
pub const FIRMWARE_EFI32: u64 = 1;
/// Booted by 64-bit UEFI.
pub const FIRMWARE_EFI64: u64 = 2;
/// Booted by a RISC-V SBI implementation.
pub const FIRMWARE_SBI: u64 = 3;

pub type FirmwareTypeRequest = Request<FirmwareTypeResponse>;

impl FirmwareTypeRequest {
    pub const fn new() -> Self {
        Request::with(FIRMWARE_TYPE_ID_2, FIRMWARE_TYPE_ID_3, ())
    }
}

#[repr(C)]
pub struct FirmwareTypeResponse {
    pub revision: u64,
    /// One of the `FIRMWARE_*` constants.
    pub firmware_type: u64,
}

// --- Date at boot request ------------------------------------------------

const DATE_AT_BOOT_ID_2: u64 = 0x502746e184c088aa;
const DATE_AT_BOOT_ID_3: u64 = 0xfbc5ec83e6327893;

pub type DateAtBootRequest = Request<DateAtBootResponse>;

impl DateAtBootRequest {
    pub const fn new() -> Self {
        Request::with(DATE_AT_BOOT_ID_2, DATE_AT_BOOT_ID_3, ())
    }
}

#[repr(C)]
pub struct DateAtBootResponse {
    pub revision: u64,
    /// Seconds since the Unix epoch, as the firmware's clock read at boot.
    pub timestamp: i64,
}

// --- Executable command line request -------------------------------------
//
// The `cmdline:` of the boot entry Limine booted, which is how a boot-menu entry selects a
// kernel mode (`hwreport`).

const EXECUTABLE_CMDLINE_ID_2: u64 = 0x4b161536e598651e;
const EXECUTABLE_CMDLINE_ID_3: u64 = 0xb390ad4a2f1f303a;

pub type ExecutableCmdlineRequest = Request<ExecutableCmdlineResponse>;

impl ExecutableCmdlineRequest {
    pub const fn new() -> Self {
        Request::with(EXECUTABLE_CMDLINE_ID_2, EXECUTABLE_CMDLINE_ID_3, ())
    }
}

#[repr(C)]
pub struct ExecutableCmdlineResponse {
    revision: u64,
    cmdline: *const u8,
}

impl ExecutableCmdlineResponse {
    /// The command line, cut at `max` bytes; empty when the entry sets none.
    pub fn line(&self, max: usize) -> &[u8] {
        // SAFETY: as `path`.
        unsafe { string(self.cmdline, max) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    /// **A request reads the response Limine wrote**: none before it answers, then that one.
    #[test]
    fn a_request_reads_the_response_limine_wrote() {
        let req = HhdmRequest::new();
        assert!(req.response().is_none());
        let mut resp = HhdmResponse { revision: 0, offset: 0xffff_8000_0000_0000 };
        req.response.store(&mut resp, Ordering::Relaxed);
        assert_eq!(req.response().map(|r| r.offset), Some(0xffff_8000_0000_0000));
    }

    /// **A Limine array is walked to its count**, its null entries passed over.
    #[test]
    fn an_array_is_walked_to_its_count_past_its_nulls() {
        let (mut a, mut b) =
            (MemoryMapEntry { base: 1, length: 2, kind: 0 }, MemoryMapEntry { base: 3, length: 4, kind: 5 });
        let mut ptrs = [&raw mut a, ptr::null_mut(), &raw mut b, &raw mut a];
        // SAFETY: the first three pointers, null or to entries that outlive the map.
        let map = unsafe { MemoryMapResponse::fabricated(3, ptrs.as_mut_ptr()) };
        assert_eq!(map.entries().map(|e| e.base).collect::<std::vec::Vec<_>>(), [1, 3]);
    }
}
