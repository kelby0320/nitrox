//! The nodes whose write acts in the call: the power node's handle, and `sys_io_submit`'s act.

use super::*;
use crate::libkern::io_op::{IoOp, IoOpcode};

#[test]
fn inits_power_node_cannot_be_given_away() {
    use crate::object::device_node::ResourceDescriptor;
    init_global_heap();
    let t = HandleTable::try_new(0x1357_9BDF_2468_ACE0).unwrap();
    let node = DeviceNode::try_new_char(ResourceDescriptor::ZERO, crate::power::BACKEND).unwrap();
    let h = t.install(1, ObjectRef::new(node), crate::power::INIT_RIGHTS).expect("INIT_RIGHTS fit a DeviceNode");
    assert!(t.lookup(h, 1, Rights::WRITE).is_ok());
    assert!(t.duplicate(h, 1, Rights::WRITE).is_err(), "a duplicate is refused");
    let held = t.lookup(h, 1, Rights::empty()).unwrap().rights;
    assert_eq!(direct_bindable(held), Err(KError::NoAccess), "and so is a bind");
    assert_eq!(direct_bindable(held | Rights::TRANSFER), Err(KError::NoAccess), "a copy wants DUPLICATE");
    assert_eq!(direct_bindable(held | Rights::DUPLICATE), Err(KError::NoAccess), "and to leave, TRANSFER");
    assert_eq!(direct_bindable(held | Rights::TRANSFER | Rights::DUPLICATE), Ok(()), "which both allow");
    drop(t.close(h, 1).unwrap());
}

#[test]
fn a_submit_to_a_node_that_acts_runs_its_act_and_makes_no_operation() {
    use crate::object::device_node::{CharBackend, ResourceDescriptor};
    const PID: u32 = 4611;
    init_global_heap();
    global::init_for_tests();
    std::thread_local! { static ACTED: core::cell::Cell<u32> = const { core::cell::Cell::new(0) }; }
    fn act(_: &MemoryObject, _: u64, _: u64) -> KError {
        ACTED.with(|n| n.set(n.get() + 1));
        KError::InvalidArgument
    }
    fn read(
        _: &Ref<MemoryObject>,
        _: &Ref<PendingOperation>,
        _: u64,
        _: u64,
        _: u64,
        _: *mut (),
    ) -> Result<(), KError> {
        Err(KError::Unsupported)
    }
    let backend = CharBackend { submit_read: read, submit_write: None, ctx: core::ptr::null_mut(), act: Some(act) };
    let node = DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).unwrap();
    let node_h = global::get().install(PID, ObjectRef::new(node), Rights::WRITE).unwrap();
    let page = global::get().install(PID, ObjectRef::new(MemoryObject::try_new(4096).unwrap()), Rights::MAP_READ);
    let op =
        IoOp { opcode: IoOpcode::Write as u32, flags: 0, buffer: page.unwrap().0, buf_offset: 0, offset: 0, length: 1 };
    assert_eq!(super::submit::submit_op(node_h.0, &op, PID), Err(KError::InvalidArgument), "its act's refusal");
    assert_eq!(ACTED.with(core::cell::Cell::get), 1);
    assert_eq!(global::close_all_owned_by(PID), 2, "the node and the page, and no operation made");
}

#[test]
fn a_write_that_acts_does_so_before_any_operation_and_only_with_bytes_to_write() {
    use crate::object::device_node::{CharBackend, ResourceDescriptor};
    init_global_heap();
    std::thread_local! { static ACTED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) }; }
    fn act(_: &MemoryObject, _: u64, len: u64) -> KError {
        ACTED.with(|n| n.set(n.get() + len));
        KError::InvalidArgument
    }
    fn read(
        _: &Ref<MemoryObject>,
        _: &Ref<PendingOperation>,
        _: u64,
        _: u64,
        _: u64,
        _: *mut (),
    ) -> Result<(), KError> {
        Err(KError::Unsupported)
    }
    let backend = CharBackend { submit_read: read, submit_write: None, ctx: core::ptr::null_mut(), act: Some(act) };
    let acting = DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).unwrap();
    let plain = DeviceNode::try_new_char(ResourceDescriptor::ZERO, CharBackend { act: None, ..backend }).unwrap();
    let buffer = MemoryObject::try_new(4096).unwrap();
    let op = |length| IoOp { opcode: IoOpcode::Write as u32, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length };
    use super::submit::acted;
    assert_eq!(acted(&acting, &buffer, &op(1), IoOpcode::Write), Some(KError::InvalidArgument));
    assert_eq!(ACTED.with(core::cell::Cell::get), 1, "it acted, once");
    assert_eq!(acted(&acting, &buffer, &op(0), IoOpcode::Write), None, "a zero-length write completes");
    assert_eq!(acted(&acting, &buffer, &op(1), IoOpcode::Read), None, "a read is not a write");
    assert_eq!(acted(&plain, &buffer, &op(1), IoOpcode::Write), None, "a node that does not act");
    assert_eq!(ACTED.with(core::cell::Cell::get), 1);
    let power = DeviceNode::try_new_char(ResourceDescriptor::ZERO, crate::power::BACKEND).unwrap();
    buffer.copy_in(0, &[7]);
    assert_eq!(acted(&power, &buffer, &op(1), IoOpcode::Write), Some(KError::InvalidArgument), "a bad power byte");
}
