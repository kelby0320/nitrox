use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
use crate::object::userspace_server::FILE_OPS_MAX;
use crate::object::{DeviceNode, IpcChannel, Producer};

fn registration() -> Ref<UserspaceServerReg> {
    let (ep, peer) = IpcChannel::try_new_pair(4).unwrap();
    drop(peer);
    UserspaceServerReg::try_new(ep).unwrap()
}

fn cached(reg: &Ref<UserspaceServerReg>) -> Ref<FileObject> {
    let device =
        Ref::new(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 7 };
    FileObject::cache_in(reg, Ref::new(FileObject::try_new(PAGE_SIZE, producer).unwrap())).unwrap()
}

fn places(reg: &Ref<UserspaceServerReg>) -> std::vec::Vec<FileOp> {
    (0..=FILE_OPS_MAX).filter_map(|_| UserspaceServerReg::begin_file_op(reg)).collect()
}

#[test]
fn a_mounts_flush_budget_refuses_one_past_it_and_another_mount_still_flushes() {
    init_global_heap();
    let (stuck, other) = (registration(), registration());
    let mut taken = places(&stuck);
    assert_eq!(taken.len(), FILE_OPS_MAX as usize, "every place, and not one past them");
    assert!(UserspaceServerReg::begin_file_op(&other).is_some(), "another mount's own");
    taken.pop();
    assert!(UserspaceServerReg::begin_file_op(&stuck).is_some(), "a place given back");
}

#[test]
fn a_flush_lets_go_of_its_file_and_place_before_it_completes() {
    init_global_heap();
    let reg = registration();
    let place = UserspaceServerReg::begin_file_op(&reg).unwrap();
    let po = Ref::new(PendingOperation::try_new().unwrap());
    let what = Some(Flushing::File(cached(&reg)));
    let job = KBox::try_new(Flush { what, place: Some(place), po: Some(po.clone()) }).unwrap();
    let (held, free) = std::thread::scope(|s| {
        s.spawn(|| Flush::run(job));
        while !crate::sched::pending_op_is_signaled(&po) {
            core::hint::spin_loop();
        }
        (held(&reg), places(&reg).len())
    });
    assert_eq!((held, free), (Ok(0), FILE_OPS_MAX as usize));
    assert_eq!(crate::sched::pending_op_completion(&po), (0, 0), "a file with nothing to write");
}

#[test]
fn a_files_flush_needs_map_write_and_names_nothing_else() {
    const PID: u32 = 4497;
    init_global_heap();
    global::init_for_tests();
    let reg = registration();
    let file = cached(&reg);
    let install = |rights| global::get().install(PID, file.clone().erase(), rights).unwrap();
    let op = |buffer| IoOp { opcode: IoOpcode::Flush as u32, flags: 0, buffer, buf_offset: 0, offset: 0, length: 0 };
    let read_only = install(Rights::MAP_READ);
    assert_eq!(super::super::submit::submit_flush(read_only.0, &op(0), PID).err(), Some(KError::NoAccess));
    let writable = install(Rights::MAP_READ | Rights::MAP_WRITE);
    assert_eq!(super::super::submit::submit_flush(writable.0, &op(7), PID).err(), Some(KError::InvalidArgument));
    assert_eq!(places(&reg).len(), FILE_OPS_MAX as usize, "no place is taken by a refusal");
    for h in [read_only, writable] {
        drop(global::get().close(h, PID).unwrap());
    }
}
