use super::*;
use crate::io::irp::{IRP_BUF_FRAGS, IrpBuffer, IrpOp, IrpStatus};

fn irp(length: u64, frags: &[PhysFrag]) -> Irp {
    let buffer = match frags.len() {
        0 => IrpBuffer::NONE,
        n => IrpBuffer { kind: IRP_BUF_FRAGS, count: n as u32, frags: frags.as_ptr() as u64 },
    };
    Irp::new_block(IrpOp::Read, core::ptr::null(), 0, length, buffer, core::ptr::null_mut(), 0)
}

fn mint(irp: &mut Irp) -> Option<InFlight> {
    // SAFETY: the test's IRP and fragments, which outlive the token.
    unsafe { InFlight::mint(NonNull::from(irp)) }
}

fn mints(length: u64, frags: &[PhysFrag]) -> bool {
    mint(&mut irp(length, frags)).map(InFlight::keep).is_some()
}

#[test]
fn a_request_whose_fragments_do_not_hold_its_length_is_not_minted() {
    let f = |len| PhysFrag { base: 0x1000, len };
    assert!(mints(1024, &[f(512), f(512)]));
    assert!(!mints(1024, &[f(512), f(511)]), "a byte short");
    assert!(!mints(1024, &[f(512), f(513)]), "a byte over");
    assert!(mints(0, &[]), "a flush");
    assert!(!mints(512, &[]), "a length and no fragments");
    assert!(!mints(0, &[f(u64::MAX), f(1)]), "a sum that wraps to its length");
}

#[test]
#[should_panic(expected = "dropped unfinished")]
fn a_request_dropped_unfinished_panics() {
    drop(mint(&mut irp(0, &[])));
}

#[test]
fn a_completed_request_is_answered_and_a_kept_one_never() {
    let _exclusive = crate::dpc::exclusive();
    let frags = [PhysFrag { base: 0x1000, len: 512 }];
    let (mut done, mut kept) = (irp(512, &frags), irp(512, &frags));
    mint(&mut done).unwrap().complete(IrpStatus::Success as i32, 512);
    mint(&mut kept).unwrap().keep();
    crate::dpc::run_pending();
    assert_eq!((done.status, done.transferred), (IrpStatus::Success as i32, 512));
    assert_eq!(kept.status, IrpStatus::Pending as i32);
}
