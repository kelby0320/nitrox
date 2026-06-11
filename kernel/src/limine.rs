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

#![allow(dead_code)]

use core::ptr;

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
        Self {
            magic_0: 0xf9562b2d5c95a6c8,
            magic_1: 0x6a7b384944536bdc,
            revision,
        }
    }

    /// `true` if the bootloader honoured our requested revision.
    pub fn supported(&self) -> bool {
        // SAFETY: `self.revision` is a plain `u64`; volatile read avoids the
        // optimiser caching the original value from before Limine zeroed it.
        unsafe { ptr::read_volatile(&self.revision) == 0 }
    }
}

/// Start-of-requests marker (4 × u64). Lives in `.limine_requests_start`.
#[repr(C)]
pub struct RequestsStartMarker(pub [u64; 4]);

impl RequestsStartMarker {
    pub const fn new() -> Self {
        Self([
            0xf6b8f4b39de7d1ae,
            0xfab91a6940fcb9cf,
            0x785c6ed015d3e316,
            0x181e920a7852b9d9,
        ])
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

// --- Framebuffer request -------------------------------------------------

const FB_ID_2: u64 = 0x9d5827dcd881dd75;
const FB_ID_3: u64 = 0xa3148604f6fab11b;

/// `memory_model` value for ordinary packed RGB framebuffers (the only kind
/// Limine emits today). Other values are reserved for future use.
pub const FB_MODEL_RGB: u8 = 1;

#[repr(C)]
pub struct FramebufferRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *mut FramebufferResponse,
}

// SAFETY: The request lives in a `static`, accessed only after Limine has
// finished writing it before jumping to the kernel. The raw pointer is read
// by a single-threaded boot context.
unsafe impl Sync for FramebufferRequest {}

impl FramebufferRequest {
    pub const fn new() -> Self {
        Self {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, FB_ID_2, FB_ID_3],
            revision: 0,
            response: ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct FramebufferResponse {
    pub revision: u64,
    pub framebuffer_count: u64,
    /// Pointer to an array of `framebuffer_count` `*mut Framebuffer`.
    pub framebuffers: *mut *mut Framebuffer,
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

#[repr(C)]
pub struct MemoryMapRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *mut MemoryMapResponse,
}

// SAFETY: identical reasoning to `FramebufferRequest` — the request lives
// in a `static`, is written exactly once by Limine before `_start`, and is
// thereafter read by single-threaded boot code.
unsafe impl Sync for MemoryMapRequest {}

impl MemoryMapRequest {
    pub const fn new() -> Self {
        Self {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, MEMMAP_ID_2, MEMMAP_ID_3],
            revision: 0,
            response: ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct MemoryMapResponse {
    pub revision: u64,
    pub entry_count: u64,
    /// Pointer to an array of `entry_count` `*mut MemoryMapEntry`.
    pub entries: *mut *mut MemoryMapEntry,
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

#[repr(C)]
pub struct HhdmRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *mut HhdmResponse,
}

// SAFETY: same single-writer, static-lifetime reasoning as the other
// requests in this file.
unsafe impl Sync for HhdmRequest {}

impl HhdmRequest {
    pub const fn new() -> Self {
        Self {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, HHDM_ID_2, HHDM_ID_3],
            revision: 0,
            response: ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct HhdmResponse {
    pub revision: u64,
    pub offset: u64,
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

#[repr(C)]
pub struct RsdpRequest {
    pub id: [u64; 4],
    pub revision: u64,
    pub response: *mut RsdpResponse,
}

// SAFETY: same single-writer, static-lifetime reasoning as the other requests
// in this file — written once by Limine before `_start`, read by single-
// threaded boot code thereafter.
unsafe impl Sync for RsdpRequest {}

impl RsdpRequest {
    pub const fn new() -> Self {
        Self {
            id: [COMMON_MAGIC_0, COMMON_MAGIC_1, RSDP_ID_2, RSDP_ID_3],
            revision: 0,
            response: ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct RsdpResponse {
    pub revision: u64,
    /// Address of the RSDP. Physical on recent Limine revisions (translate via
    /// the HHDM); may be an HHDM-virtual pointer on older bootloaders.
    pub address: u64,
}
