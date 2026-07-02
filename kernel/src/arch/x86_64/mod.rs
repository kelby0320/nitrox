//! x86_64-specific primitives.

pub mod abi;
pub mod acpi;
pub mod apic;
pub mod context;
pub mod cpu;
pub mod entropy;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod paging;
pub mod registers;
pub mod regs;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod timer;
pub mod tlb;
pub mod user_access;

// CPU control (init_tables / init_protections / set_kernel_stack / halt_loop)
// lives on the `ArchCpu` trait, impl'd in `cpu.rs` and re-exported as
// `crate::arch::Cpu`.
