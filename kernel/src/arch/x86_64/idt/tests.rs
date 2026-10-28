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
