use super::*;

/// Vector 2 is the only vector that can be a stop notice, and only during a stop: a
/// hardware NMI outside one is a real fault, and any other vector during a stop is a fault
/// the stopping CPU took on its way down.
#[test]
fn is_stop_notice_is_vector_2_during_a_stop_and_nothing_else() {
    use super::is_stop_notice;

    assert!(is_stop_notice(2, true), "vector 2 during a stop is the notice");
    assert!(!is_stop_notice(2, false), "a hardware NMI outside a stop is a real fault");
    for v in [0u64, 1, 3, 6, 8, 13, 14, 32, 255] {
        assert!(!is_stop_notice(v, true), "vector {v} during a stop is not the notice");
        assert!(!is_stop_notice(v, false), "vector {v} outside a stop is not the notice");
    }
}

#[test]
fn set_handler_splits_address() {
    let mut e = IdtEntry::EMPTY;
    e.set_handler(0x1234_5678_9ABC_DEF0, 1);
    assert_eq!(e.offset_low, 0xDEF0);
    assert_eq!(e.offset_mid, 0x9ABC);
    assert_eq!(e.offset_high, 0x1234_5678);
    assert_eq!(e.selector, KERNEL_CODE_SELECTOR);
    assert_eq!(e.ist, 1);
    assert_eq!(e.type_attr, 0x8E);
}

#[test]
fn set_handler_masks_ist_to_three_bits() {
    let mut e = IdtEntry::EMPTY;
    e.set_handler(0, 0xFF);
    assert_eq!(e.ist, 0x7);
}

#[test]
fn pf_fault_kind_decodes_error_bits() {
    assert_eq!(pf_fault_kind(0b0000), FaultKind::NotMapped); // not present
    assert_eq!(pf_fault_kind(0b0001), FaultKind::NotReadable); // present, read
    assert_eq!(pf_fault_kind(0b0011), FaultKind::NotWritable); // present, write
    assert_eq!(pf_fault_kind(0b1_0001), FaultKind::NotExecutable); // present, insn fetch
    assert_eq!(pf_fault_kind(0b1_0010), FaultKind::NotMapped);
}

#[test]
fn fault_shape_maps_vectors() {
    assert_eq!(fault_shape(0, 0, 0xAAAA, None), (KIND_DIVIDE_BY_ZERO, 0xAAAA, FaultKind::UnknownFault));
    assert_eq!(fault_shape(6, 0, 0xBBBB, None).0, KIND_ILLEGAL_INSN);
    let (k, addr, fk) = fault_shape(14, 0b0011, 0xCCCC, Some(0x1000));
    assert_eq!((k, addr, fk), (KIND_SEG_FAULT, 0x1000, FaultKind::NotWritable));
    assert_eq!(fault_shape(13, 0, 0xDDDD, None), (KIND_SEG_FAULT, 0xDDDD, FaultKind::UnknownFault));
}

#[test]
fn stub_table_covers_all_32_vectors() {
    assert_eq!(STUBS.len(), 32);
}

/// **The entry flag clear clears DF and AC**, run here with both set first. Ring 3 may set AC with
/// `popfq` as it may DF, which is how a stub can find them, and how this test runs the stubs' text.
#[test]
#[cfg_attr(miri, ignore = "inline assembly")]
fn the_entry_flag_clear_leaves_df_and_ac_clear() {
    const DF_AC: u64 = 1 << 10 | 1 << 18;
    let (set, cleared): (u64, u64);
    // SAFETY: sets DF and AC, runs the clear, then clears both again apart from the macro, as the
    // end of an `asm!` block requires; every push is popped, and each access aligned, as AC needs.
    unsafe {
        core::arch::asm!(
            "pushfq",
            "or dword ptr [rsp], 0x40400",
            "popfq",
            "pushfq",
            "pop {set}",
            clear_entry_flags!(),
            "pushfq",
            "pop {cleared}",
            "pushfq",
            "and qword ptr [rsp], -0x40401",
            "popfq",
            "cld",
            set = out(reg) set,
            cleared = out(reg) cleared,
        );
    }
    assert_eq!(set & DF_AC, DF_AC, "the test set both");
    assert_eq!(cleared & DF_AC, 0, "the clear left {:#x}", cleared & DF_AC);
}

/// **A vector delivered on an IST stack is never a user fault**, from ring 3 or not: suspending
/// would park the thread, and its context, on the per-CPU stack the next NMI or #DF reuses.
#[test]
fn an_nmi_or_double_fault_from_ring_3_is_not_a_user_fault() {
    let frame = |vector, cs| {
        // SAFETY: every field is a `u64`, for which zero is valid.
        let mut f: ExceptionFrame = unsafe { core::mem::zeroed() };
        (f.vector, f.cs) = (vector, cs);
        f
    };
    for vector in [2, 8] {
        assert!(!is_user_fault(&frame(vector, 0x23)), "vector {vector} from ring 3");
    }
    for vector in [0, 6, 13, 14] {
        assert!(is_user_fault(&frame(vector, 0x23)), "vector {vector} from ring 3");
        assert!(!is_user_fault(&frame(vector, 0x08)), "vector {vector} from ring 0");
    }
    assert!(is_user_nmi(&frame(2, 0x23)) && !is_user_nmi(&frame(2, 0x08)) && !is_user_nmi(&frame(8, 0x23)));
}
