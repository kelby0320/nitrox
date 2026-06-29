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
//! **Per-CPU.** Each CPU has its own GDT, TSS, and `#DF` stack (the [`GDTS`],
//! [`TSSES`], [`DF_STACKS`] arrays, indexed by [`this_cpu`]), because a TSS holds
//! per-CPU stacks (IST1 and RSP0). [`init`] runs once per CPU — the BSP at boot,
//! each AP during its own bring-up — and loads that CPU's tables.
//!
//! Layout of each GDT: null, 64-bit kernel code (`0x08`), kernel data (`0x10`),
//! ring-3 data (`0x18`), ring-3 code (`0x20`), then a 16-byte TSS descriptor
//! occupying two slots (`0x28`). The user-selector order is fixed by
//! `syscall`/`sysret` (see [`STAR_VALUE`]).

use core::arch::asm;

use crate::arch::smp::MAX_CPUS;
use crate::arch::x86_64::regs;

/// Dense index of the running CPU (its per-CPU GDT/TSS slot), read via `RDTSCP`
/// — the same source as `X86Smp::current_cpu`. Valid from the first instruction
/// (`IA32_TSC_AUX` resets to 0, so the BSP reads 0 even before `init_this_cpu`);
/// an AP sets its index (via `init_this_cpu`) *before* calling [`init`].
fn this_cpu() -> usize {
    let cpu = regs::rdtscp_aux() as usize;
    debug_assert!(cpu < MAX_CPUS, "cpu index out of range");
    cpu
}

/// Selector for the 64-bit kernel code segment (GDT index 1). The IDT's
/// gates reference this; see `idt.rs`.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// Selector for the kernel data segment (GDT index 2).
const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector for the ring-3 data segment (GDT index 3), with RPL 3. Loaded
/// into SS by `sysretq` (and pushed as SS in the ring-3 `iretq` frame).
pub const USER_DATA_SELECTOR: u16 = 0x18 | 3;
/// Selector for the ring-3 64-bit code segment (GDT index 4), with RPL 3.
/// Loaded into CS by `sysretq` (and pushed as CS in the `iretq` frame).
pub const USER_CODE_SELECTOR: u16 = 0x20 | 3;
/// Selector for the TSS descriptor (GDT index 5, spanning indices 5-6).
const TSS_SELECTOR: u16 = 0x28;

/// 64-bit kernel code descriptor: present, DPL 0, executable, long-mode.
const KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
/// Kernel data descriptor: present, DPL 0, writable.
const KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;
/// Ring-3 data descriptor: kernel data with DPL 3 (access `0x92 | 0x60 =
/// 0xF2`).
const USER_DATA: u64 = 0x00CF_F200_0000_FFFF;
/// Ring-3 64-bit code descriptor: kernel code with DPL 3 (access `0x9A |
/// 0x60 = 0xFA`).
const USER_CODE: u64 = 0x00AF_FA00_0000_FFFF;

/// The `IA32_STAR` value for `syscall`/`sysretq`.
///
/// - `STAR[47:32]` (the `syscall` base) = `0x08`: `syscall` loads
///   `CS = 0x08` (kernel code) and `SS = 0x08 + 8 = 0x10` (kernel data).
/// - `STAR[63:48]` (the `sysret` base) = `0x10`: `sysretq` loads
///   `SS = 0x10 + 8 = 0x18` and `CS = 0x10 + 16 = 0x20` (each with RPL
///   forced to 3, giving `0x1B` / `0x23`). This is *why* the GDT places
///   user data at `0x18` and user code at `0x20`, in that order.
pub const STAR_VALUE: u64 = (0x0010u64 << 48) | (0x0008u64 << 32);

/// GDT slot count: null + kernel code + kernel data + user data + user
/// code + TSS descriptor (two slots).
const GDT_LEN: usize = 7;

/// Size of the per-CPU double-fault IST stack.
const DF_STACK_SIZE: usize = 16 * 1024;

/// One Global Descriptor Table **per CPU**, indexed by [`this_cpu`]; each CPU
/// loads its own (the TSS descriptor's base is the runtime address of that CPU's
/// [`TSSES`] slot). Populated by [`init`]. Only slot 0 is live until APs start.
static mut GDTS: [[u64; GDT_LEN]; MAX_CPUS] = [[0; GDT_LEN]; MAX_CPUS];

/// One 64-bit Task State Segment **per CPU**. Each CPU's `ist[0]` (IST1) points
/// at its own `#DF` stack, and `rsp[0]` at its current thread's kernel stack.
static mut TSSES: [Tss; MAX_CPUS] = [const { Tss::new(0) }; MAX_CPUS];

/// Backing storage for the per-CPU double-fault IST stacks. The CPU loads RSP
/// directly from IST1 on a `#DF`, so 16-byte alignment is baked in.
static mut DF_STACKS: [DfStack; MAX_CPUS] = [const { DfStack([0; DF_STACK_SIZE]) }; MAX_CPUS];

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
    let cpu = this_cpu();
    // Raw element pointers into this CPU's per-CPU statics — no bounds-check
    // panic in the boot path, and each CPU touches only its own slots.
    // SAFETY: `cpu < MAX_CPUS` (a dense id from `this_cpu`); `add(cpu)` stays in
    // bounds. No references into the `static mut`s are formed.
    let (df_ptr, tss_ptr, gdt_ptr) = unsafe {
        (
            (&raw const DF_STACKS).cast::<DfStack>().add(cpu),
            (&raw mut TSSES).cast::<Tss>().add(cpu),
            (&raw mut GDTS).cast::<[u64; GDT_LEN]>().add(cpu),
        )
    };

    // 1. Point IST1 at the top of this CPU's double-fault stack (stacks grow
    //    down) and write the completed TSS.
    let df_top = (df_ptr as usize as u64) + DF_STACK_SIZE as u64;
    let tss = Tss::new(df_top);
    // SAFETY: `tss_ptr` is this CPU's live, correctly-sized `Tss` slot, owned
    // exclusively by this CPU with no outstanding reference.
    unsafe {
        tss_ptr.write(tss);
    }

    // 2. Build this CPU's GDT with a TSS descriptor for the TSS just written.
    let tss_addr = tss_ptr as usize as u64;
    let tss_desc = tss_descriptor(tss_addr, (size_of::<Tss>() - 1) as u32);
    // Order is fixed by `sysretq`: user data (0x18) then user code (0x20),
    // with the TSS pushed to 0x28. See `STAR_VALUE`.
    let gdt: [u64; GDT_LEN] = [
        0,
        KERNEL_CODE, // 0x08
        KERNEL_DATA, // 0x10
        USER_DATA,   // 0x18
        USER_CODE,   // 0x20
        tss_desc[0], // 0x28 (low)
        tss_desc[1], // 0x28 (high)
    ];
    // SAFETY: `gdt_ptr` is this CPU's exclusively-owned GDT slot, no outstanding
    // reference.
    unsafe {
        gdt_ptr.write(gdt);
    }

    // 3. Load this CPU's GDT, reload the segment registers, load its TSS.
    let ptr = GdtPointer {
        limit: (size_of::<[u64; GDT_LEN]>() - 1) as u16,
        base: gdt_ptr as usize as u64,
    };
    // SAFETY: `ptr` describes the GDT just populated; the selector constants
    // match the table indices; `TSS_SELECTOR` indexes the TSS descriptor written
    // in step 2.
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

/// Set `TSS.RSP0` — the stack the CPU loads when an interrupt or exception
/// is taken while running in ring 3. Must be set before any ring-3 entry.
///
/// Note: the `syscall` instruction does *not* consult RSP0 (it doesn't
/// switch stacks at all — the syscall entry stub loads the kernel stack
/// itself via the per-CPU block); RSP0 covers a fault/IRQ taken in ring 3.
pub fn set_kernel_stack(top: u64) {
    let cpu = this_cpu();
    // SAFETY: writes only the running CPU's TSS slot (`cpu < MAX_CPUS`), which it
    // owns exclusively with no outstanding reference. `rsp[0]` is a `u64` at a
    // 4-aligned offset in a `#[repr(C, packed)]` TSS, so write it unaligned to
    // avoid forming a misaligned reference.
    unsafe {
        let tss_ptr = (&raw mut TSSES).cast::<Tss>().add(cpu);
        let rsp0 = &raw mut (*tss_ptr).rsp[0];
        rsp0.write_unaligned(top);
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

    #[test]
    fn user_descriptors_are_kernel_descriptors_with_dpl3() {
        // Same base/limit/flags as the kernel descriptors, access byte
        // raised to DPL 3 (the `0x60` DPL bits).
        assert_eq!(USER_CODE, KERNEL_CODE | (0x60 << 40), "user code = kcode|DPL3");
        assert_eq!(USER_DATA, KERNEL_DATA | (0x60 << 40), "user data = kdata|DPL3");
        assert_eq!((USER_CODE >> 40) & 0xFF, 0xFA, "user code access byte");
        assert_eq!((USER_DATA >> 40) & 0xFF, 0xF2, "user data access byte");
    }

    #[test]
    fn star_value_yields_the_sysret_and_syscall_selectors() {
        let sysret_base = STAR_VALUE >> 48;
        let syscall_base = (STAR_VALUE >> 32) & 0xFFFF;
        // syscall: CS = base, SS = base + 8 → kernel code / kernel data.
        assert_eq!(syscall_base, KERNEL_CODE_SELECTOR as u64);
        assert_eq!(syscall_base + 8, KERNEL_DATA_SELECTOR as u64);
        // sysretq: SS = base + 8, CS = base + 16 → user data / user code
        // (RPL stripped; the hardware forces RPL 3 on load).
        assert_eq!(sysret_base + 8, (USER_DATA_SELECTOR & !3) as u64);
        assert_eq!(sysret_base + 16, (USER_CODE_SELECTOR & !3) as u64);
    }
}
