//! The kernel's own Global Descriptor Table, Task State Segment, and the
//! IST stack the double-fault handler runs on.
//!
//! Limine hands the kernel a working GDT, but the kernel needs its own:
//! the IDT's gates must reference a code selector the kernel controls,
//! and a reliable `#DF` handler needs an Interrupt Stack Table entry —
//! which lives in a TSS, which needs a TSS descriptor in a GDT the kernel
//! owns. So the GDT, the TSS, and the IST double-fault stack are brought
//! up together by [`init`].
//!
//! Layout of [`GDT`]: null, 64-bit kernel code (`0x08`), kernel data
//! (`0x10`), and a 16-byte TSS descriptor occupying two slots (`0x18`).
//! User-mode selectors are deliberately omitted — there is no userspace
//! yet, and they must be ordered for `syscall`/`sysret` when it lands.

use core::arch::asm;

/// Selector for the 64-bit kernel code segment (GDT index 1). The IDT's
/// gates reference this; see `idt.rs`.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// Selector for the kernel data segment (GDT index 2).
const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector for the TSS descriptor (GDT index 3, spanning indices 3-4).
const TSS_SELECTOR: u16 = 0x18;

/// 64-bit kernel code descriptor: present, DPL 0, executable, long-mode.
const KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
/// Kernel data descriptor: present, DPL 0, writable.
const KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;

/// GDT slot count: null + code + data + TSS descriptor (two slots).
const GDT_LEN: usize = 5;

/// Size of the per-CPU double-fault IST stack.
const DF_STACK_SIZE: usize = 16 * 1024;

/// The Global Descriptor Table. Populated by [`init`]; `static mut`
/// because the TSS descriptor's base is the runtime address of [`TSS`].
static mut GDT: [u64; GDT_LEN] = [0; GDT_LEN];

/// The 64-bit Task State Segment. Phase 1 uses only `ist[0]` (IST1), the
/// stack the double-fault handler runs on.
static mut TSS: Tss = Tss::new(0);

/// Backing storage for the double-fault IST stack. The CPU loads RSP
/// directly from IST1 on a `#DF`, so 16-byte alignment is baked in.
static mut DF_STACK: DfStack = DfStack([0; DF_STACK_SIZE]);

/// A 16-byte-aligned block of bytes used as an exception stack.
#[repr(C, align(16))]
struct DfStack([u8; DF_STACK_SIZE]);

/// The 64-bit Task State Segment, exact hardware layout.
///
/// `#[repr(C, packed)]` is mandatory: the architectural TSS places 64-bit
/// fields at 4-byte-aligned offsets (a `u32` sits at offset 0), which a
/// naturally-aligned `#[repr(C)]` would pad and corrupt.
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    /// RSP0-2 — privilege-level stacks. Unused until userspace exists.
    rsp: [u64; 3],
    reserved1: u64,
    /// IST1-7 — interrupt-stack-table entries. Only IST1 is used.
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// Offset of the I/O permission bitmap. Set to the TSS size so the
    /// CPU treats every port as not-permitted (the kernel never traps on
    /// it because it runs in ring 0).
    iomap_base: u16,
}

const _: () = assert!(size_of::<Tss>() == 104);

impl Tss {
    /// Construct a zeroed TSS whose IST1 entry is `ist1`. `const` so it
    /// can initialise the [`TSS`] static; [`init`] rewrites it with the
    /// real stack address.
    const fn new(ist1: u64) -> Self {
        Tss {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [ist1, 0, 0, 0, 0, 0, 0],
            reserved2: 0,
            reserved3: 0,
            iomap_base: size_of::<Tss>() as u16,
        }
    }
}

/// The operand of `lgdt`: table byte-length minus one, then base address.
#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Encode a 64-bit TSS descriptor (a 16-byte system descriptor occupying
/// two GDT slots) for a TSS at `base` with byte-limit `limit`.
///
/// Pure arithmetic, so it is exercised by host tests below.
fn tss_descriptor(base: u64, limit: u32) -> [u64; 2] {
    let mut low: u64 = 0;
    low |= (limit as u64) & 0xFFFF; // limit 15:0
    low |= (base & 0xFF_FFFF) << 16; // base 23:0
    low |= 0x89_u64 << 40; // access: present, DPL 0, available 64-bit TSS
    low |= (((limit as u64) >> 16) & 0xF) << 48; // limit 19:16
    low |= ((base >> 24) & 0xFF) << 56; // base 31:24
    let high: u64 = (base >> 32) & 0xFFFF_FFFF; // base 63:32
    [low, high]
}

/// Install the kernel's GDT, load the TSS, and switch to kernel
/// selectors. Call once, early in boot, before [`crate::arch::x86_64::idt::init`].
pub fn init() {
    // 1. Point IST1 at the top of the double-fault stack (stacks grow
    //    down) and write the completed TSS.
    let df_top = (&raw const DF_STACK as usize as u64) + DF_STACK_SIZE as u64;
    let tss = Tss::new(df_top);
    // SAFETY: boot is single-threaded; `TSS` is a 'static this module
    // owns exclusively, and no reference into it is outstanding. The
    // pointer is to a live, correctly-sized `Tss`.
    unsafe {
        (&raw mut TSS).write(tss);
    }

    // 2. Build the GDT with a TSS descriptor for the TSS just written.
    let tss_addr = &raw const TSS as usize as u64;
    let tss_desc = tss_descriptor(tss_addr, (size_of::<Tss>() - 1) as u32);
    let gdt: [u64; GDT_LEN] = [0, KERNEL_CODE, KERNEL_DATA, tss_desc[0], tss_desc[1]];
    // SAFETY: as above — `GDT` is an exclusively-owned 'static with no
    // outstanding reference.
    unsafe {
        (&raw mut GDT).write(gdt);
    }

    // 3. Load the GDT, reload the segment registers, load the TSS.
    let ptr = GdtPointer {
        limit: (size_of::<[u64; GDT_LEN]>() - 1) as u16,
        base: &raw const GDT as usize as u64,
    };
    // SAFETY: `ptr` describes the GDT just populated; the selector
    // constants match the table indices; `TSS_SELECTOR` indexes the TSS
    // descriptor written in step 2.
    unsafe {
        load_gdt(&ptr);
        reload_segments();
        load_tss();
    }
}

/// Execute `lgdt` against `ptr`.
///
/// # Safety
/// `ptr` must describe a valid, fully-populated GDT.
unsafe fn load_gdt(ptr: &GdtPointer) {
    // SAFETY: the caller guarantees `ptr` points at a valid GDT operand.
    // `lgdt` reads 10 bytes from it and updates GDTR; it touches no flags
    // and uses no stack.
    unsafe {
        asm!("lgdt [{}]", in(reg) ptr, options(readonly, nostack, preserves_flags));
    }
}

/// Reload `CS` (via a far return — `mov cs, ...` is illegal in long mode)
/// and the data-segment registers to the kernel selectors.
///
/// # Safety
/// A GDT with kernel code at `0x08` and kernel data at `0x10` must be
/// loaded.
unsafe fn reload_segments() {
    // SAFETY: with the kernel GDT loaded, `0x08`/`0x10` are valid kernel
    // selectors. The far return pops a new `CS:RIP` pair pushed just
    // above; the data-segment writes cannot fault for a valid selector.
    unsafe {
        asm!(
            "push {code}",            // new CS
            "lea {scratch}, [rip + 2f]", // new RIP
            "push {scratch}",
            "retfq",                  // far return: pops RIP, then CS
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov fs, {data:x}",
            "mov gs, {data:x}",
            "mov ss, {data:x}",
            code = in(reg) KERNEL_CODE_SELECTOR as u64,
            data = in(reg) KERNEL_DATA_SELECTOR as u64,
            scratch = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// Execute `ltr` to load the task register with the TSS selector.
///
/// # Safety
/// The GDT must contain a valid TSS descriptor at `TSS_SELECTOR`.
unsafe fn load_tss() {
    // SAFETY: `TSS_SELECTOR` indexes the TSS descriptor populated in
    // `init`. `ltr` marks that descriptor busy and loads TR.
    unsafe {
        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tss_descriptor_encodes_base_and_limit() {
        // Base spread across all four base fields; limit across both.
        let [low, high] = tss_descriptor(0x1234_5678_9ABC_DEF0, 0x6_7104);

        assert_eq!(low & 0xFFFF, 0x7104, "limit 15:0");
        assert_eq!((low >> 16) & 0xFF_FFFF, 0xBC_DEF0, "base 23:0");
        assert_eq!((low >> 40) & 0xFF, 0x89, "access byte");
        assert_eq!((low >> 48) & 0xF, 0x6, "limit 19:16");
        assert_eq!((low >> 56) & 0xFF, 0x9A, "base 31:24");
        assert_eq!(high, 0x1234_5678, "base 63:32");
    }

    #[test]
    fn tss_descriptor_zero_is_all_zero_but_access() {
        let [low, high] = tss_descriptor(0, 0);
        assert_eq!(low, 0x89_u64 << 40);
        assert_eq!(high, 0);
    }
}
