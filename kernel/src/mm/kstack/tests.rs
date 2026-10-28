use super::*;
use crate::arch::Paging;
use crate::arch::paging::ArchPaging;
use crate::mm::PhysAddr;
use crate::mm::addr_space::AddressSpace;
use crate::mm::heap;
use crate::mm::test_support::init_global_heap;

fn translate(root: PhysAddr, va: VirtAddr) -> Option<PhysAddr> {
    // SAFETY: a read-only walk of a root this test holds.
    unsafe { Paging::translate(root, va) }
}

/// Writable, not executable and the kernel's alone, under every root.
#[test]
fn a_stack_is_mapped_under_every_root_above_an_unmapped_guard() {
    init_global_heap();
    let before = AddressSpace::new().unwrap();
    let stack = KernelStack::new().unwrap();
    let after = AddressSpace::new().unwrap();
    for root in [before.root(), after.root()] {
        for i in 0..KERNEL_STACK_PAGES as u64 {
            let v = VirtAddr::new(stack.base().as_u64() + i * PAGE_SIZE as u64);
            // SAFETY: a read-only walk of a root this test holds.
            let ok = unsafe { crate::arch::mapped_with(root, v, PageFlags::WRITABLE | PageFlags::NO_EXECUTE) };
            assert!(ok, "stack page {i}");
        }
        assert!(translate(root, stack.guard_page()).is_none(), "the guard page");
    }
}

#[test]
fn top_is_base_plus_stack_bytes_above_a_guard_page() {
    init_global_heap();
    let stack = KernelStack::new().unwrap();
    assert_eq!(stack.top().as_u64() - stack.base().as_u64(), KERNEL_STACK_BYTES);
    assert_eq!(stack.base().as_u64() - stack.guard_page().as_u64(), PAGE_SIZE as u64);
}

#[test]
fn multiple_stacks_have_disjoint_ranges() {
    init_global_heap();
    let s1 = KernelStack::new().unwrap();
    let s2 = KernelStack::new().unwrap();
    let (lo, hi) = if s1.base().as_u64() < s2.base().as_u64() { (&s1, &s2) } else { (&s2, &s1) };
    assert!(lo.top().as_u64() <= hi.guard_page().as_u64(), "stacks overlap");
}

#[test]
fn a_stack_outlives_the_address_space_it_was_made_under() {
    init_global_heap();
    let made_under = AddressSpace::new().unwrap();
    let stack = KernelStack::new().unwrap();
    let other = AddressSpace::new().unwrap();
    drop(made_under);
    assert!(translate(other.root(), stack.base()).is_some());
    let (shots, freed) = (crate::tlb::shootdowns_by_this_thread().0, heap::frames_freed_by_this_thread());
    drop(stack);
    // Not by its VA, which another test's stack may already have.
    assert_eq!(crate::tlb::shootdowns_by_this_thread(), (shots + 1, freed), "shot down before a frame is freed");
    assert_eq!(heap::frames_freed_by_this_thread() - freed, KERNEL_STACK_PAGES);
}
