//! In-kernel ELF64 loader for static binaries.
//!
//! Populates a fresh [`AddressSpace`] from an ELF byte slice: parses
//! the header, walks the program headers, allocates a VMA for each
//! `PT_LOAD` segment (with the segment's protection), copies the file
//! bytes into the newly-allocated frames via the HHDM, and finishes
//! with an initial stack VMA at a fixed top-of-user-space address. The
//! caller receives an [`EntryInfo`] with the entry point and stack
//! top, ready to hand to whatever launches the process (when threading
//! and syscall entry exist).
//!
//! Architecture-neutral: the only arch-specific values are pulled
//! from [`crate::arch::abi`] — the ELF `e_machine` for the host
//! architecture, the user-half upper bound, and the default user
//! stack placement. The aarch64 port supplies its own values; this
//! file is unchanged.
//!
//! ## Scope today
//!
//! - **Static ELF64 little-endian only**, machine matching the host
//!   architecture. `ET_DYN` is rejected (PIE handling needs base
//!   randomization, which is a separate sub-item). `PT_INTERP` is
//!   rejected: dynamic linking is a userspace concern handled by a
//!   future `ld.so`-equivalent, matching the universal
//!   kernel/userspace boundary (Linux `binfmt_elf` / Windows NTDLL /
//!   macOS dyld). Little-endian only is a Nitrox-wide convention
//!   (the project doesn't target BE configurations even where the
//!   architecture allows them).
//! - **No argv / envp / auxv setup on the stack.** Nitrox passes
//!   argv/env as typed structural values rather than C strings; the
//!   handoff format belongs to the "first userspace process"
//!   milestone, where the userspace runtime defines it.
//! - **No partial-load rollback.** If a segment fails to map midway
//!   through `load_elf`, the address space is left in a partial state.
//!   The caller is expected to drop it — [`AddressSpace::Drop`] tears
//!   down any successfully-installed VMAs and reclaims their frames.
//!
//! ## Where the bytes come from
//!
//! `load_elf` takes a `&[u8]` and is agnostic to its source. Today the
//! only realistic source is a binary embedded via `include_bytes!`
//! (the kernel knows the init binary at compile time). When the
//! initramfs subsystem arrives (Phase 2) the same loader will accept
//! bytes read from CPIO.

use crate::arch::abi::{
    DEFAULT_USER_STACK_SIZE as STACK_SIZE, DEFAULT_USER_STACK_TOP as STACK_TOP, E_MACHINE,
    USER_VIRT_END,
};
use crate::arch::Paging;
use crate::arch::paging::ArchPaging;
use crate::libkern::KBox;
use crate::mm::addr_space::{AddressSpace, MapError};
use crate::mm::heap;
use crate::mm::vmm::{MappingKind, Protection, VAddrRange, Vma};
use crate::mm::{PAGE_SIZE, VirtAddr};

// ----- ELF64 constants (arch-neutral; spec values) -----

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const EI_CLASS_64: u8 = 2;
const EI_DATA_LSB: u8 = 1;
const EI_VERSION_CURRENT: u8 = 1;

const E_TYPE_EXEC: u16 = 2;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;
// PF_R is implicit: every architecture Nitrox targets makes a present
// mapping readable (x86_64 has no separate read bit; aarch64's AP
// encoding always permits read at the corresponding EL).

const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: usize = 56;

// ----- Public API -----

/// Why [`load_elf`] could not populate the address space.
#[derive(Debug, PartialEq, Eq)]
pub enum ElfLoadError {
    /// The slice ends before a required field, segment, or referenced
    /// file offset.
    Truncated,
    BadMagic,
    Not64Bit,
    NotLittleEndian,
    NotCurrentVersion,
    /// `e_machine` did not match the host architecture's
    /// [`crate::arch::abi::E_MACHINE`].
    WrongMachine,
    /// `e_type` was not `ET_EXEC`. `ET_DYN` (PIE) is rejected until
    /// base-address randomization lands.
    NotExecutable,
    /// `PT_INTERP` is present. Dynamic linking is a userspace concern.
    HasInterpreter,
    /// A `PT_LOAD` segment violated `p_vaddr % PAGE == p_offset % PAGE`.
    BadSegmentAlignment,
    /// A `PT_LOAD` segment falls outside `[0, USER_VIRT_END)`, has a
    /// non-canonical endpoint, or `p_filesz > p_memsz`.
    BadSegmentRange,
    /// Two `PT_LOAD` segments overlap (or one overlaps the initial
    /// stack region).
    SegmentOverlap,
    /// Any heap or page-table allocation failed.
    OutOfMemory,
}

/// Entry information returned by a successful [`load_elf`]. The caller
/// passes these to whatever launches the user thread once threading
/// and syscall entry exist.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct EntryInfo {
    pub entry_point: VirtAddr,
    pub stack_top: VirtAddr,
}

/// Parse `bytes` as an ELF64 binary and populate `asp` with its
/// `PT_LOAD` segments and an initial stack VMA.
///
/// On success, returns the entry point and the (page-aligned, exclusive)
/// stack top. On failure, `asp` may have been partially populated —
/// the caller should drop it.
pub fn load_elf(asp: &AddressSpace, bytes: &[u8]) -> Result<EntryInfo, ElfLoadError> {
    let ehdr = parse_ehdr(bytes)?;

    // Walk program headers: detect PT_INTERP (reject), load PT_LOAD,
    // ignore everything else.
    for i in 0..ehdr.e_phnum {
        let off = ehdr
            .e_phoff
            .checked_add((i as u64).wrapping_mul(ELF64_PHDR_SIZE as u64))
            .ok_or(ElfLoadError::Truncated)?;
        let phdr = parse_phdr(bytes, off as usize)?;
        match phdr.p_type {
            PT_INTERP => return Err(ElfLoadError::HasInterpreter),
            PT_LOAD => map_load_segment(asp, bytes, &phdr)?,
            _ => continue,
        }
    }

    // Reserve the initial stack VMA **lazily**: no frames are allocated here
    // — each page is faulted in (zeroed) on first touch by
    // `AddressSpace::fault_in`. The stack is the clean demand-paging candidate:
    // pure zero-fill, with no loader-written content (the userspace entry is
    // register-based — no argv/envp/auxv is placed on the stack). PT_LOAD
    // segments below stay eager because the loader copies file bytes into their
    // frames. See docs/architecture and docs/rationale/deferred-decisions.md.
    let stack_range = VAddrRange::new(
        VirtAddr::new(STACK_TOP - STACK_SIZE),
        VirtAddr::new(STACK_TOP),
    )
    .expect("stack range constants are valid by construction");
    let stack_vma = KBox::try_new(Vma::new(
        stack_range,
        Protection::WRITE | Protection::USER,
        MappingKind::Anonymous,
    ))
    .map_err(|_| ElfLoadError::OutOfMemory)?;
    match asp.map_vma_lazy(stack_vma) {
        Ok(()) => {}
        // map_vma_lazy allocates nothing, so OutOfMemory cannot occur; the
        // arm remains for exhaustiveness over MapError.
        Err((_, MapError::OutOfMemory)) => return Err(ElfLoadError::OutOfMemory),
        Err((_, MapError::Overlap)) => return Err(ElfLoadError::SegmentOverlap),
        Err((_, MapError::NotCanonical | MapError::NotUserHalf)) => {
            unreachable!("stack range is fixed and validated at compile time")
        }
    }

    Ok(EntryInfo {
        entry_point: VirtAddr::new(ehdr.e_entry),
        stack_top: VirtAddr::new(STACK_TOP),
    })
}

// ----- Parsed header types -----

struct Ehdr {
    e_entry: u64,
    e_phoff: u64,
    e_phnum: u16,
}

struct Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
}

// ----- Parsing -----

fn parse_ehdr(bytes: &[u8]) -> Result<Ehdr, ElfLoadError> {
    if bytes.len() < ELF64_EHDR_SIZE {
        return Err(ElfLoadError::Truncated);
    }
    // e_ident validation.
    if bytes[0..4] != ELF_MAGIC {
        return Err(ElfLoadError::BadMagic);
    }
    if bytes[4] != EI_CLASS_64 {
        return Err(ElfLoadError::Not64Bit);
    }
    if bytes[5] != EI_DATA_LSB {
        return Err(ElfLoadError::NotLittleEndian);
    }
    if bytes[6] != EI_VERSION_CURRENT {
        return Err(ElfLoadError::NotCurrentVersion);
    }
    let e_type = read_u16(bytes, 16)?;
    let e_machine = read_u16(bytes, 18)?;
    let e_entry = read_u64(bytes, 24)?;
    let e_phoff = read_u64(bytes, 32)?;
    let e_phentsize = read_u16(bytes, 54)?;
    let e_phnum = read_u16(bytes, 56)?;
    if e_machine != E_MACHINE {
        return Err(ElfLoadError::WrongMachine);
    }
    if e_type != E_TYPE_EXEC {
        return Err(ElfLoadError::NotExecutable);
    }
    // e_phentsize is supposed to be exactly the Phdr size in the spec.
    // Tolerate it being missing (zero phnum) but otherwise insist.
    if e_phnum != 0 && (e_phentsize as usize) != ELF64_PHDR_SIZE {
        return Err(ElfLoadError::Truncated);
    }
    Ok(Ehdr {
        e_entry,
        e_phoff,
        e_phnum,
    })
}

fn parse_phdr(bytes: &[u8], off: usize) -> Result<Phdr, ElfLoadError> {
    if bytes.len() < off + ELF64_PHDR_SIZE {
        return Err(ElfLoadError::Truncated);
    }
    Ok(Phdr {
        p_type: read_u32(bytes, off)?,
        p_flags: read_u32(bytes, off + 4)?,
        p_offset: read_u64(bytes, off + 8)?,
        p_vaddr: read_u64(bytes, off + 16)?,
        p_filesz: read_u64(bytes, off + 32)?,
        p_memsz: read_u64(bytes, off + 40)?,
    })
}

fn read_u16(bytes: &[u8], off: usize) -> Result<u16, ElfLoadError> {
    let slice: [u8; 2] = bytes
        .get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfLoadError::Truncated)?;
    Ok(u16::from_le_bytes(slice))
}

fn read_u32(bytes: &[u8], off: usize) -> Result<u32, ElfLoadError> {
    let slice: [u8; 4] = bytes
        .get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfLoadError::Truncated)?;
    Ok(u32::from_le_bytes(slice))
}

fn read_u64(bytes: &[u8], off: usize) -> Result<u64, ElfLoadError> {
    let slice: [u8; 8] = bytes
        .get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfLoadError::Truncated)?;
    Ok(u64::from_le_bytes(slice))
}

// ----- Segment loading -----

fn map_load_segment(
    asp: &AddressSpace,
    bytes: &[u8],
    phdr: &Phdr,
) -> Result<(), ElfLoadError> {
    let page = PAGE_SIZE as u64;

    // p_vaddr % PAGE == p_offset % PAGE is mandatory: it's the only
    // way to copy the file bytes contiguously through the HHDM (the
    // bytes' page-relative offset must match the virtual address's).
    if (phdr.p_vaddr % page) != (phdr.p_offset % page) {
        return Err(ElfLoadError::BadSegmentAlignment);
    }
    if phdr.p_filesz > phdr.p_memsz {
        return Err(ElfLoadError::BadSegmentRange);
    }

    // Page-aligned VMA range covering [p_vaddr, p_vaddr + p_memsz).
    let v_end = phdr
        .p_vaddr
        .checked_add(phdr.p_memsz)
        .ok_or(ElfLoadError::BadSegmentRange)?;
    let aligned_start = phdr.p_vaddr & !(page - 1);
    let aligned_end = v_end
        .checked_add(page - 1)
        .ok_or(ElfLoadError::BadSegmentRange)?
        & !(page - 1);
    if aligned_end > USER_VIRT_END {
        return Err(ElfLoadError::BadSegmentRange);
    }
    let range = VAddrRange::new(VirtAddr::new(aligned_start), VirtAddr::new(aligned_end))
        .ok_or(ElfLoadError::BadSegmentRange)?;

    // Translate ELF p_flags to VMA Protection. R is implicit.
    let mut prot = Protection::USER;
    if phdr.p_flags & PF_W != 0 {
        prot = prot | Protection::WRITE;
    }
    if phdr.p_flags & PF_X != 0 {
        prot = prot | Protection::EXEC;
    }

    let vma = KBox::try_new(Vma::new(range, prot, MappingKind::Anonymous))
        .map_err(|_| ElfLoadError::OutOfMemory)?;
    match asp.map_vma(vma) {
        Ok(()) => {}
        Err((_, MapError::OutOfMemory)) => return Err(ElfLoadError::OutOfMemory),
        Err((_, MapError::Overlap)) => return Err(ElfLoadError::SegmentOverlap),
        Err((_, MapError::NotCanonical | MapError::NotUserHalf)) => {
            return Err(ElfLoadError::BadSegmentRange);
        }
    }

    // Copy the file bytes into the newly-mapped frames. The HHDM lets
    // us write to any physical frame as `phys + hhdm_offset()`.
    // Anything beyond p_filesz up to p_memsz is BSS — already zero
    // from map_vma's anonymous-allocation step.
    if phdr.p_filesz == 0 {
        return Ok(());
    }
    let file_end = phdr
        .p_offset
        .checked_add(phdr.p_filesz)
        .ok_or(ElfLoadError::Truncated)?;
    if (file_end as usize) > bytes.len() {
        return Err(ElfLoadError::Truncated);
    }

    let root = asp.root();
    let mut va = phdr.p_vaddr;
    let mut file_off = phdr.p_offset;
    let copy_end = phdr.p_vaddr + phdr.p_filesz;
    while va < copy_end {
        let next_page = (va & !(page - 1)) + page;
        let chunk = core::cmp::min(next_page, copy_end) - va;

        // SAFETY: `root` is the AS's valid PML4; `va` lies in a range
        // we just mapped (validated `aligned_start..aligned_end`
        // covers `[p_vaddr, p_vaddr + p_memsz)`), so translate must
        // succeed.
        let phys = unsafe { Paging::translate(root, VirtAddr::new(va)) }
            .expect("just-mapped virtual address must translate");

        // SAFETY: `phys` is a freshly-allocated frame owned by `asp`;
        // HHDM gives a writable view; `chunk` does not exceed the
        // remainder of the page.
        unsafe {
            let dst = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
            let src = bytes[file_off as usize..(file_off + chunk) as usize].as_ptr();
            core::ptr::copy_nonoverlapping(src, dst, chunk as usize);
        }

        va += chunk;
        file_off += chunk;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    // ----- Test ELF builder -----

    struct ElfBuilder {
        segments: Vec<TestSegment>,
        e_type: u16,
        e_machine: u16,
        e_class: u8,
        e_data: u8,
        e_version: u8,
        e_entry: u64,
    }

    struct TestSegment {
        p_type: u32,
        p_flags: u32,
        p_vaddr: u64,
        p_memsz: u64,
        data: Vec<u8>,
    }

    impl ElfBuilder {
        fn new() -> Self {
            Self {
                segments: Vec::new(),
                e_type: E_TYPE_EXEC,
                e_machine: E_MACHINE,
                e_class: EI_CLASS_64,
                e_data: EI_DATA_LSB,
                e_version: EI_VERSION_CURRENT,
                e_entry: 0x400000,
            }
        }

        fn entry(mut self, v: u64) -> Self {
            self.e_entry = v;
            self
        }
        fn e_type(mut self, v: u16) -> Self {
            self.e_type = v;
            self
        }
        fn machine(mut self, v: u16) -> Self {
            self.e_machine = v;
            self
        }
        fn class(mut self, v: u8) -> Self {
            self.e_class = v;
            self
        }
        fn data(mut self, v: u8) -> Self {
            self.e_data = v;
            self
        }
        fn version(mut self, v: u8) -> Self {
            self.e_version = v;
            self
        }

        fn load_segment(mut self, p_vaddr: u64, p_flags: u32, data: Vec<u8>, p_memsz: u64) -> Self {
            self.segments.push(TestSegment {
                p_type: PT_LOAD,
                p_flags,
                p_vaddr,
                p_memsz,
                data,
            });
            self
        }
        fn interp_segment(mut self, p_vaddr: u64) -> Self {
            self.segments.push(TestSegment {
                p_type: PT_INTERP,
                p_flags: 0,
                p_vaddr,
                p_memsz: 0,
                data: Vec::new(),
            });
            self
        }

        fn build(self) -> Vec<u8> {
            // Layout: Ehdr | Phdrs | (padding to page boundary) | seg0 | seg1 | ...
            // Each segment's data starts at a file offset matching its
            // p_vaddr mod PAGE to satisfy the ELF alignment rule.
            let phdr_count = self.segments.len();
            let phdr_bytes = phdr_count * ELF64_PHDR_SIZE;
            let mut bytes = Vec::new();
            bytes.resize(ELF64_EHDR_SIZE + phdr_bytes, 0);

            // Place segment data; record each offset.
            let mut seg_offsets = Vec::with_capacity(phdr_count);
            for seg in &self.segments {
                // Choose offset so p_offset % PAGE == p_vaddr % PAGE.
                let target = (seg.p_vaddr as usize) & 0xFFF;
                while bytes.len() & 0xFFF != target {
                    bytes.push(0);
                }
                let offset = bytes.len();
                bytes.extend_from_slice(&seg.data);
                seg_offsets.push(offset as u64);
            }

            // Fill Ehdr.
            bytes[0..4].copy_from_slice(&ELF_MAGIC);
            bytes[4] = self.e_class;
            bytes[5] = self.e_data;
            bytes[6] = self.e_version;
            bytes[16..18].copy_from_slice(&self.e_type.to_le_bytes());
            bytes[18..20].copy_from_slice(&self.e_machine.to_le_bytes());
            bytes[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
            bytes[24..32].copy_from_slice(&self.e_entry.to_le_bytes());
            bytes[32..40].copy_from_slice(&(ELF64_EHDR_SIZE as u64).to_le_bytes()); // e_phoff
            // e_shoff (40..48): 0
            // e_flags (48..52): 0
            bytes[52..54].copy_from_slice(&(ELF64_EHDR_SIZE as u16).to_le_bytes()); // e_ehsize
            bytes[54..56].copy_from_slice(&(ELF64_PHDR_SIZE as u16).to_le_bytes()); // e_phentsize
            bytes[56..58].copy_from_slice(&(phdr_count as u16).to_le_bytes()); // e_phnum
            // e_shentsize, e_shnum, e_shstrndx: 0

            // Fill Phdrs.
            for (i, seg) in self.segments.iter().enumerate() {
                let base = ELF64_EHDR_SIZE + i * ELF64_PHDR_SIZE;
                bytes[base..base + 4].copy_from_slice(&seg.p_type.to_le_bytes());
                bytes[base + 4..base + 8].copy_from_slice(&seg.p_flags.to_le_bytes());
                bytes[base + 8..base + 16].copy_from_slice(&seg_offsets[i].to_le_bytes());
                bytes[base + 16..base + 24].copy_from_slice(&seg.p_vaddr.to_le_bytes());
                // p_paddr (24..32): 0
                bytes[base + 32..base + 40]
                    .copy_from_slice(&(seg.data.len() as u64).to_le_bytes()); // p_filesz
                bytes[base + 40..base + 48].copy_from_slice(&seg.p_memsz.to_le_bytes());
                bytes[base + 48..base + 56]
                    .copy_from_slice(&(PAGE_SIZE as u64).to_le_bytes()); // p_align
            }

            bytes
        }
    }

    fn read_user_byte(asp: &AddressSpace, virt: VirtAddr) -> u8 {
        let phys = unsafe { Paging::translate(asp.root(), virt) }
            .expect("address must be mapped for the test to read it");
        unsafe { *((phys.as_u64() + heap::hhdm_offset()) as *const u8) }
    }

    // ----- Tests -----

    #[test]
    fn truncated_input_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        assert_eq!(load_elf(&asp, &[]), Err(ElfLoadError::Truncated));
        assert_eq!(load_elf(&asp, &[0u8; 32]), Err(ElfLoadError::Truncated));
    }

    #[test]
    fn bad_magic_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let bad = vec![0x42; ELF64_EHDR_SIZE];
        assert_eq!(load_elf(&asp, &bad), Err(ElfLoadError::BadMagic));
    }

    #[test]
    fn wrong_class_or_data_or_version_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let bytes = ElfBuilder::new().class(1).build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::Not64Bit));
        let bytes = ElfBuilder::new().data(2).build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::NotLittleEndian));
        let bytes = ElfBuilder::new().version(0).build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::NotCurrentVersion));
    }

    #[test]
    fn wrong_machine_or_type_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // Any value that differs from the host's E_MACHINE rejects.
        let bytes = ElfBuilder::new()
            .machine(E_MACHINE.wrapping_add(1))
            .build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::WrongMachine));
        let bytes = ElfBuilder::new().e_type(3).build(); // ET_DYN
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::NotExecutable));
    }

    #[test]
    fn pt_interp_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let bytes = ElfBuilder::new().interp_segment(0x500000).build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::HasInterpreter));
    }

    #[test]
    fn single_load_segment_maps_and_copies_bytes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // Page-aligned base, 0x80 bytes of file content, 0x100 total
        // memsz so there's some BSS to verify zero-init.
        let mut content = Vec::with_capacity(0x80);
        for i in 0..0x80 {
            content.push((i as u8).wrapping_mul(7));
        }
        let bytes = ElfBuilder::new()
            .entry(0x400010)
            .load_segment(0x400000, PF_R, content.clone(), 0x100)
            .build();

        let info = load_elf(&asp, &bytes).expect("load must succeed");
        assert_eq!(info.entry_point, VirtAddr::new(0x400010));
        assert_eq!(info.stack_top, VirtAddr::new(STACK_TOP));

        // Bytes inside p_filesz match the file content.
        for i in 0..0x80u64 {
            let got = read_user_byte(&asp, VirtAddr::new(0x400000 + i));
            assert_eq!(got, content[i as usize], "mismatch at byte {i}");
        }
        // BSS bytes past p_filesz are zero.
        for i in 0x80u64..0x100 {
            let got = read_user_byte(&asp, VirtAddr::new(0x400000 + i));
            assert_eq!(got, 0, "BSS byte {i:#x} should be zero");
        }
    }

    #[test]
    fn segment_with_non_zero_in_page_offset_copies_correctly() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // p_vaddr at 0x400100, file_offset arranged to match (mod
        // PAGE). The first page of the VMA covers [0x400000, 0x401000),
        // and bytes 0x100..0x180 of that page must hold the content.
        let mut content = Vec::new();
        for i in 0..0x80 {
            content.push(0xA0 | (i as u8));
        }
        let bytes = ElfBuilder::new()
            .entry(0x400100)
            .load_segment(0x400100, PF_R | PF_W, content.clone(), 0x80)
            .build();
        load_elf(&asp, &bytes).expect("load must succeed");

        // Bytes below p_vaddr in the VMA's first page are zero (we
        // mapped the page, but the file data starts at offset 0x100).
        for i in 0..0x100u64 {
            let got = read_user_byte(&asp, VirtAddr::new(0x400000 + i));
            assert_eq!(got, 0, "pre-segment byte {i:#x} should be zero");
        }
        // Content area matches.
        for i in 0..0x80u64 {
            let got = read_user_byte(&asp, VirtAddr::new(0x400100 + i));
            assert_eq!(got, content[i as usize]);
        }
    }

    #[test]
    fn multi_page_segment_spans_correctly() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // A segment that spans two pages.
        let size = (PAGE_SIZE as u64) + 0x200;
        let mut content = Vec::with_capacity(size as usize);
        for i in 0..size {
            content.push((i & 0xFF) as u8);
        }
        let bytes = ElfBuilder::new()
            .load_segment(0x400000, PF_R | PF_W, content.clone(), size)
            .build();
        load_elf(&asp, &bytes).expect("load must succeed");

        // Sample first byte, last-of-first-page, first-of-second-page,
        // last byte.
        let probes: &[u64] = &[
            0,
            (PAGE_SIZE as u64) - 1,
            PAGE_SIZE as u64,
            size - 1,
        ];
        for &off in probes {
            let got = read_user_byte(&asp, VirtAddr::new(0x400000 + off));
            assert_eq!(got, content[off as usize], "mismatch at offset {off:#x}");
        }
    }

    #[test]
    fn segment_alignment_violation_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // Build manually with mismatched p_offset / p_vaddr mod PAGE.
        let mut bytes = ElfBuilder::new()
            .load_segment(0x400000, PF_R, vec![0u8; 4], 4)
            .build();
        // Patch p_vaddr to 0x400123 (no matching adjustment to
        // p_offset, which the builder placed at PAGE-aligned).
        let phdr_base = ELF64_EHDR_SIZE;
        let bad_vaddr = 0x400123u64;
        bytes[phdr_base + 16..phdr_base + 24].copy_from_slice(&bad_vaddr.to_le_bytes());
        assert_eq!(
            load_elf(&asp, &bytes),
            Err(ElfLoadError::BadSegmentAlignment)
        );
    }

    #[test]
    fn segment_in_kernel_half_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // p_vaddr in the kernel half.
        let bytes = ElfBuilder::new()
            .load_segment(0xFFFF_8000_0000_0000, PF_R, vec![0u8; 4], 4)
            .build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::BadSegmentRange));
    }

    #[test]
    fn overlapping_segments_rejected() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // Two PT_LOAD segments that share a page.
        let bytes = ElfBuilder::new()
            .load_segment(0x400000, PF_R, vec![0u8; 0x100], 0x100)
            .load_segment(0x400500, PF_R | PF_W, vec![0u8; 0x100], 0x100)
            .build();
        assert_eq!(load_elf(&asp, &bytes), Err(ElfLoadError::SegmentOverlap));
    }

    #[test]
    fn stack_vma_is_reserved_lazily_at_fixed_top_of_user_space() {
        use crate::mm::addr_space::FaultIn;
        use crate::mm::vmm::FaultAccess;
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let bytes = ElfBuilder::new()
            .load_segment(0x400000, PF_R | PF_W, vec![0xCC; 0x40], 0x40)
            .build();
        let info = load_elf(&asp, &bytes).expect("load must succeed");
        assert_eq!(info.stack_top, VirtAddr::new(STACK_TOP));

        // The stack is reserved lazily — its pages are NOT backed until first
        // touch, so nothing translates yet.
        let last_stack_byte = VirtAddr::new(STACK_TOP - 1);
        let first_stack_byte = VirtAddr::new(STACK_TOP - STACK_SIZE);
        assert!(
            unsafe { Paging::translate(asp.root(), last_stack_byte) }.is_none(),
            "stack must be demand-paged, not eagerly backed"
        );

        // Faulting a stack page in (as a real first stack write would) yields a
        // freshly zero-filled page; an as-yet-untouched page stays unbacked.
        assert_eq!(asp.fault_in(last_stack_byte, FaultAccess::Write), FaultIn::Mapped);
        assert_eq!(read_user_byte(&asp, last_stack_byte), 0);
        assert!(
            unsafe { Paging::translate(asp.root(), first_stack_byte) }.is_none(),
            "an untouched stack page must remain unbacked"
        );
    }

    /// The ELF `PF_R` (readable) bit. The loader doesn't consult it
    /// (every mapping is readable on x86_64), but realistic test ELFs
    /// set it on every `PT_LOAD` so we mirror that here.
    const PF_R: u32 = 4;
}
