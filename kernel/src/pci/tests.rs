use super::*;
use core::cell::RefCell;

struct FakeCfg {
    normal: RefCell<[u32; 64]>,
    sizing: [u32; 64],
    armed: RefCell<[bool; 64]>,
}

impl FakeCfg {
    fn new() -> Self {
        FakeCfg { normal: RefCell::new([0; 64]), sizing: [0; 64], armed: RefCell::new([false; 64]) }
    }
    fn set(&mut self, off: u16, val: u32) {
        self.normal.borrow_mut()[(off / 4) as usize] = val;
    }
    fn set_sizing(&mut self, off: u16, mask: u32) {
        self.sizing[(off / 4) as usize] = mask;
    }
}

impl Cfg for FakeCfg {
    fn read32(&self, off: u16) -> u32 {
        let idx = (off / 4) as usize;
        if self.armed.borrow()[idx] { self.sizing[idx] } else { self.normal.borrow()[idx] }
    }
    fn write32(&self, off: u16, val: u32) {
        let idx = (off / 4) as usize;
        if val == 0xFFFF_FFFF {
            self.armed.borrow_mut()[idx] = true;
        } else {
            self.normal.borrow_mut()[idx] = val;
            self.armed.borrow_mut()[idx] = false;
        }
    }
}

fn with_msi(off: u16, ctl: u16, next: u8) -> FakeCfg {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, (STATUS_CAP_LIST << 16) | 0x0006);
    c.set(REG_CAP_PTR, off as u32);
    c.set(off, CAP_ID_MSI as u32 | ((next as u32) << 8) | ((ctl as u32) << 16));
    c
}

/// QEMU's ICH9 AHCI, read off a live boot: MSI at `0x80`, 64-bit (control `0x0080`).
fn qemu_ahci_msi() -> FakeCfg {
    with_msi(0x80, 0x0080, 0xA8)
}

/// The laptop's Sunrise Point-LP AHCI, from `lspci -vv`: MSI at `0x80`, 32-bit (control `0`).
fn laptop_ahci_msi() -> FakeCfg {
    with_msi(0x80, 0x0000, 0xA8)
}

#[test]
fn the_report_names_each_ahci_controllers_msi_form() {
    let mut laptop = laptop_ahci_msi();
    laptop.set(REG_CAP_PTR, 0x70);
    laptop.set(0x70, 0x01 | (0x80 << 8));
    laptop.set(0xA8, 0x12);
    assert_eq!(format!("{}", Capabilities::read(&laptop)), "caps msi32 +2");

    let mut qemu = qemu_ahci_msi();
    qemu.set(0xA8, 0x12);
    assert_eq!(format!("{}", Capabilities::read(&qemu)), "caps msi64 +1");
}

#[test]
fn the_report_names_msix_pcie_and_masking_and_counts_the_rest() {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
    c.set(REG_CAP_PTR, 0x50);
    c.set(0x50, 0x01 | (0x70 << 8)); // power management
    c.set(0x70, CAP_ID_MSI as u32 | (0x90 << 8) | (0x0180 << 16)); // 64-bit, per-vector mask
    c.set(0x90, CAP_ID_MSIX as u32 | (0xA0 << 8));
    c.set(0xA0, CAP_ID_PCIE as u32);
    assert_eq!(format!("{}", Capabilities::read(&c)), "caps msi64/mask msix pcie +1");
}

#[test]
fn the_report_says_none_without_a_list_and_flags_an_msi_the_driver_would_refuse() {
    let mut none = qemu_ahci_msi();
    none.set(REG_COMMAND, 0x0006);
    assert_eq!(format!("{}", Capabilities::read(&none)), "caps none");

    let malformed = with_msi(0xF4, 0x0080, 0);
    assert_eq!(read_msi(&malformed), None);
    assert_eq!(format!("{}", Capabilities::read(&malformed)), "caps msi-malformed");
}

#[test]
fn the_capability_walk_follows_the_chain_to_a_later_entry() {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
    c.set(REG_CAP_PTR, 0x70);
    c.set(0x70, 0x01 | (0x80 << 8)); // power management -> 0x80
    c.set(0x80, CAP_ID_MSI as u32 | (0xA8 << 8));
    c.set(0xA8, 0x12); // SATA, end of chain
    assert_eq!(find_capability(&c, CAP_ID_MSI), Some(0x80));
    assert_eq!(find_capability(&c, 0x12), Some(0xA8), "the last entry is reachable");
    assert_eq!(find_capability(&c, 0x11), None, "MSI-X is not in this chain");
}

#[test]
fn the_capability_walk_declines_when_the_status_bit_is_clear() {
    let mut c = qemu_ahci_msi();
    c.set(REG_COMMAND, 0x0006);
    assert_eq!(find_capability(&c, CAP_ID_MSI), None);
}

#[test]
fn the_capability_walk_terminates_on_a_circular_chain() {
    let mut ok = FakeCfg::new();
    ok.set(REG_COMMAND, STATUS_CAP_LIST << 16);
    ok.set(REG_CAP_PTR, 0x40);
    ok.set(0x40, 0x01 | (0x50 << 8));
    ok.set(0x50, 0x10);
    assert_eq!(find_capability(&ok, 0x10), Some(0x50), "control: a sane chain resolves");

    let mut loopy = ok;
    loopy.set(0x50, 0x10 | (0x40 << 8));
    assert_eq!(find_capability(&loopy, CAP_ID_MSI), None);
}

#[test]
fn the_capability_walk_rejects_a_pointer_outside_the_capability_region() {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
    c.set(REG_CAP_PTR, 0x10); // inside the header — that is BAR0, not a capability
    // Decodable on purpose, so only the range check refuses it.
    c.set(0x10, CAP_ID_MSI as u32);
    assert_eq!(find_capability(&c, CAP_ID_MSI), None);
}

#[test]
fn an_msi_capability_that_would_run_past_the_header_is_refused() {
    let c = with_msi(0xF4, 0x0080, 0);
    assert_eq!(find_capability(&c, CAP_ID_MSI), Some(0xF4), "the walk finds it");
    assert_eq!(read_msi(&c), None, "and read_msi refuses it");

    let ok = with_msi(0xF0, 0x0080, 0);
    assert!(read_msi(&ok).is_some(), "0xF0 leaves exactly enough room");

    let narrow = with_msi(0xF4, 0x0000, 0);
    assert!(read_msi(&narrow).is_some(), "0xF4 fits a 32-bit structure");
}

#[test]
fn the_two_target_controllers_decode_to_different_shapes() {
    let qemu = read_msi(&qemu_ahci_msi()).expect("QEMU's AHCI advertises MSI");
    assert_eq!(qemu.off, 0x80);
    assert!(qemu.addr64, "QEMU's ICH9 AHCI is 64-bit");
    assert!(!qemu.per_vector_mask, "and non-maskable");
    assert_eq!(qemu.multi_message_capable, 0, "one vector");

    let laptop = read_msi(&laptop_ahci_msi()).expect("the laptop's AHCI advertises MSI");
    assert_eq!(laptop.off, 0x80, "at the same offset, which is the trap");
    assert!(!laptop.addr64, "the laptop's Sunrise Point-LP AHCI is 32-bit");
    assert!(!laptop.per_vector_mask);
    assert_eq!(laptop.multi_message_capable, 0);
}

/// No QEMU boot reaches the 32-bit branch (QEMU's AHCI is 64-bit); a driver shaped by the
/// emulator would write Data at `+0x0C` instead of `+0x08`.
#[test]
fn a_32_bit_capability_takes_its_data_at_plus_8_and_nothing_beyond() {
    let c = laptop_ahci_msi();
    let cap = read_msi(&c).unwrap();
    assert_eq!(msi_data_offset(&cap), 0x88);
    // SAFETY: a synthetic config space; nothing is armed by this write.
    assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });

    assert_eq!(c.read32(0x84), 0xFEE0_0000, "message address");
    assert_eq!(c.read32(0x88) & 0xFFFF, 0x0031, "message data, at +0x08");
    assert_eq!(
        c.read32(0x8C),
        0,
        "+0x0C is past the end of a 32-bit non-maskable capability and must \
         not be written — on the laptop it is reserved space before the SATA \
         capability at 0xA8"
    );
}

#[test]
fn a_64_bit_capability_takes_the_upper_address_at_plus_8_and_data_at_plus_c() {
    let c = qemu_ahci_msi();
    let cap = read_msi(&c).unwrap();
    assert_eq!(msi_data_offset(&cap), 0x8C);
    // SAFETY: a synthetic config space.
    assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });

    assert_eq!(c.read32(0x84), 0xFEE0_0000, "message address, low half");
    assert_eq!(c.read32(0x88), 0, "message address, upper half");
    assert_eq!(c.read32(0x8C) & 0xFFFF, 0x0031, "message data, at +0x0C");
}

#[test]
fn programming_msi_preserves_the_reserved_half_of_the_data_dword() {
    let mut c = laptop_ahci_msi();
    c.set(0x88, 0xDEAD_0000); // reserved upper half, arbitrarily non-zero
    let cap = read_msi(&c).unwrap();
    // SAFETY: a synthetic config space.
    assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });
    assert_eq!(c.read32(0x88), 0xDEAD_0031, "16-bit field, read-modify-write");
}

#[test]
fn programming_msi_enables_exactly_one_vector() {
    // MME arrives already 3, so clearing it is work the write must do (0x00B6).
    let c = with_msi(0x80, 0x00B6, 0);
    let cap = read_msi(&c).unwrap();
    assert_eq!(cap.multi_message_capable, 3, "capable of 8");
    assert_ne!(
        (c.read32(0x80) >> 16) as u16 & MSI_CTL_MME_MASK,
        0,
        "precondition: the fixture has multiple messages enabled"
    );
    // SAFETY: a synthetic config space.
    assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0030) });

    let ctl = (c.read32(0x80) >> 16) as u16;
    assert_ne!(ctl & MSI_CTL_ENABLE, 0, "enabled");
    assert_eq!(ctl & MSI_CTL_MME_MASK, 0, "multiple message enable = 1 vector");
    assert_eq!(c.read32(0x80) & 0xFF, CAP_ID_MSI as u32, "capability id preserved");
    assert_eq!((c.read32(0x80) >> 8) & 0xFF, 0, "next pointer preserved");
}

#[test]
fn a_32_bit_capability_refuses_an_address_it_cannot_hold_and_writes_nothing() {
    let c = laptop_ahci_msi();
    let cap = read_msi(&c).unwrap();
    // SAFETY: a synthetic config space.
    let wrote = unsafe { program_msi(&c, &cap, 0x1_FEE0_0000, 0x0031) };
    assert!(!wrote, "refused rather than truncated to 0xFEE0_0000");
    assert_eq!(c.read32(0x84), 0, "message address untouched");
    assert_eq!(c.read32(0x88), 0, "message data untouched");
    assert_eq!((c.read32(0x80) >> 16) as u16 & MSI_CTL_ENABLE, 0, "and not enabled");

    let wide = qemu_ahci_msi();
    let wide_cap = read_msi(&wide).unwrap();
    // SAFETY: a synthetic config space.
    assert!(unsafe { program_msi(&wide, &wide_cap, 0x1_FEE0_0000, 0x0031) });
    assert_eq!(wide.read32(0x88), 1, "upper half of the address");
}

#[test]
fn writing_the_command_register_never_writes_ones_into_the_status_half() {
    // `FakeCfg` has no write-1-to-clear semantics: what is checked is the value written.
    const LATCHED: u32 = (1 << 13) | (1 << 15); // target abort, parity error
    for (what, run) in [
        (
            "enable_bus_master",
            &(|c: &FakeCfg| {
                enable_bus_master(c);
            }) as &dyn Fn(&FakeCfg),
        ),
        ("set_intx_disabled", &(|c: &FakeCfg| set_intx_disabled(c, true))),
        (
            "size_bars",
            &(|c: &FakeCfg| {
                size_bars(c);
            }),
        ),
    ] {
        let mut c = FakeCfg::new();
        c.set(REG_COMMAND, 0x0003 | (LATCHED << 16));
        run(&c);
        assert_eq!(
            c.read32(REG_COMMAND) >> 16,
            0,
            "{what} wrote the status half back; on hardware that clears the \
             abort and parity bits a first bring-up is diagnosed from"
        );
    }
}

#[test]
fn enabling_bus_master_reports_whether_the_firmware_had_already_done_it() {
    let mut already = FakeCfg::new();
    already.set(REG_COMMAND, 0x0007);
    assert!(enable_bus_master(&already), "firmware had set it");
    assert_eq!(already.read32(REG_COMMAND) & 0xFFFF, 0x0007, "unchanged");

    let mut not_yet = FakeCfg::new();
    not_yet.set(REG_COMMAND, 0x0003);
    assert!(!enable_bus_master(&not_yet), "firmware had not");
    assert_eq!(not_yet.read32(REG_COMMAND) & 0xFFFF, 0x0007, "now it is set");
}

#[test]
fn intx_disable_is_its_own_bit_and_leaves_the_decode_bits_alone() {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, 0x0007);
    set_intx_disabled(&c, true);
    assert_eq!(c.read32(REG_COMMAND) & 0xFFFF, 0x0407, "bit 10 set, 0..2 intact");
    set_intx_disabled(&c, false);
    assert_eq!(c.read32(REG_COMMAND) & 0xFFFF, 0x0007, "and it is reversible");
}

fn ahci_fake() -> FakeCfg {
    let mut c = FakeCfg::new();
    c.set(REG_ID, 0x2922_8086); // vendor 8086, device 2922
    c.set(REG_COMMAND, 0x0000_0007); // I/O + mem + bus-master enabled
    c.set(REG_CLASS_REV, 0x0106_0102); // class 01, subclass 06, prog_if 01, rev 02
    c.set(REG_HEADER, 0x0000_0000); // header type 0, single-function
    c.set(REG_BAR0 + 5 * 4, 0xFEBF_1000);
    c.set_sizing(REG_BAR0 + 5 * 4, 0xFFFF_E000);
    c.set(REG_INTERRUPT, 0x0000_0105); // line 5, pin 1 (INTA)
    c
}

#[test]
fn decodes_ahci_identity() {
    let id = decode_identity(&ahci_fake());
    assert_eq!(id.vendor, 0x8086);
    assert_eq!(id.device, 0x2922);
    assert_eq!(id.class, 0x01);
    assert_eq!(id.subclass, 0x06);
    assert_eq!(id.prog_if, 0x01);
    assert_eq!(id.revision, 0x02);
}

#[test]
fn sizes_32bit_mmio_abar() {
    let bars = size_bars(&ahci_fake());
    let abar = &bars[5];
    assert_eq!(abar.kind, BAR_MMIO);
    assert_eq!(abar.base, 0xFEBF_1000);
    assert_eq!(abar.size, 0x2000);
    assert_eq!(abar.flags, 0); // 32-bit, non-prefetchable
    assert_eq!(bars[0].kind, BAR_NONE);
}

#[test]
fn size_bars_restores_command_register() {
    let cfg = ahci_fake();
    let before = cfg.read32(REG_COMMAND);
    let _ = size_bars(&cfg);
    assert_eq!(cfg.read32(REG_COMMAND), before, "decode-enable bits not restored");
}

#[test]
fn sizes_64bit_prefetchable_mmio() {
    let mut c = FakeCfg::new();
    c.set(REG_HEADER, 0); // header type 0
    c.set(REG_BAR0, 0x8000_000C); // bits: mem(0)=0, type(2:1)=10, prefetch(3)=1
    c.set(REG_BAR0 + 4, 0x0000_0000);
    // As hardware answers: the type bits kept below the size mask.
    c.set_sizing(REG_BAR0, 0xFFF0_000C);
    c.set_sizing(REG_BAR0 + 4, 0xFFFF_FFFF);
    let bars = size_bars(&c);
    assert_eq!(bars[0].kind, BAR_MMIO);
    assert_eq!(bars[0].base, 0x8000_0000);
    assert_eq!(bars[0].size, 0x10_0000);
    assert_eq!(bars[0].flags, BAR_FLAG_64 | BAR_FLAG_PREFETCH);
    assert_eq!(bars[1].kind, BAR_NONE);
}

/// **A function of a multifunction device has its BARs sized**: header type `0x80` is type 0.
#[test]
fn a_multifunction_devices_function_has_its_bars() {
    let mut c = ahci_fake();
    c.set(REG_HEADER, 0x0080_0000);
    let bars = decode_function(&c, 0, 0, 0x1F, 2).bars;
    assert_eq!((bars[5].kind, bars[5].base, bars[5].size), (BAR_MMIO, 0xFEBF_1000, 0x2000));
}

/// **I/O BARs of four and eight bytes, and a 32-bit prefetchable one**, each sized from a read-back
/// that keeps its type bits, as hardware returns it.
#[test]
fn small_io_bars_and_a_prefetchable_one_size_from_a_hardware_read_back() {
    let mut c = FakeCfg::new();
    c.set(REG_BAR0, 0xC005);
    c.set_sizing(REG_BAR0, 0xFFFF_FFFD);
    c.set(REG_BAR0 + 4, 0xC009);
    c.set_sizing(REG_BAR0 + 4, 0xFFFF_FFF9);
    c.set(REG_BAR0 + 8, 0xFEBF_1008);
    c.set_sizing(REG_BAR0 + 8, 0xFFFF_E008);
    let bars = size_bars(&c);
    assert_eq!((bars[0].kind, bars[0].base, bars[0].size), (BAR_IO, 0xC004, 4));
    assert_eq!((bars[1].kind, bars[1].base, bars[1].size), (BAR_IO, 0xC008, 8));
    assert_eq!((bars[2].base, bars[2].size, bars[2].flags), (0xFEBF_1000, 0x2000, BAR_FLAG_PREFETCH));
}

#[test]
fn reads_interrupt_line_and_pin() {
    let int = read_interrupt(&ahci_fake());
    assert_eq!(int.line, 5);
    assert_eq!(int.pin, 1);
    assert_eq!(int.present, 1);
}

#[test]
fn absent_function_has_no_pin() {
    let mut c = FakeCfg::new();
    c.set(REG_INTERRUPT, 0x0000_0000);
    let int = read_interrupt(&c);
    assert_eq!(int.pin, 0);
    assert_eq!(int.present, 0);
}

fn pm_fake(pmcsr: u32) -> FakeCfg {
    let mut c = FakeCfg::new();
    c.set(REG_COMMAND, (STATUS_CAP_LIST << 16) | 0x0406);
    c.set(REG_CAP_PTR, 0x70);
    c.set(0x70, CAP_ID_PM as u32 | 0x0003 << 16); // last in the chain; PM 1.2
    c.set(0x74, pmcsr);
    c.set(REG_BAR0, 0xB131_0004);
    c.set(REG_BAR0 + 4, 0x0000_0001);
    c
}

#[test]
fn a_function_in_d0_is_left_alone() {
    let c = pm_fake(0x0008);
    assert_eq!(power_up(&c).0, Power::AlreadyD0);
    assert_eq!(c.read32(0x74), 0x0008, "nothing written");
    let mut none = FakeCfg::new();
    none.set(REG_COMMAND, 0);
    assert_eq!(power_up(&none).0, Power::NoCapability);
}

/// **The laptop's xHCI as Linux left it** (`D3 NoSoftRst+ PME-Enable+`): raised to D0.
#[test]
fn d3_is_raised_to_d0_without_clearing_pme_status() {
    let c = pm_fake(PMCSR_PME_STATUS | 1 << 8 | PMCSR_NO_SOFT_RESET | 0b11);
    assert_eq!(power_up(&c).0, Power::Raised { from: 3, kept: true });
    let after = c.read32(0x74);
    assert_eq!(after & PMCSR_STATE, 0, "D0");
    assert_eq!(after & PMCSR_PME_STATUS, 0, "a one here would clear PME_Status");
    assert_ne!(after & (1 << 8), 0, "PME_Enable kept");
}

#[test]
fn a_resetting_transition_is_undone_by_the_save() {
    let mut c = pm_fake(0b11);
    let (power, saved) = power_up(&c);
    assert_eq!(power, Power::Raised { from: 3, kept: false });
    c.set(REG_BAR0, 0);
    c.set(REG_BAR0 + 4, 0);
    c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
    saved.restore(&c);
    assert_eq!(c.read32(REG_BAR0), 0xB131_0004);
    assert_eq!(c.read32(REG_BAR0 + 4), 0x0000_0001);
    assert_eq!(c.read32(REG_COMMAND) & 0xFFFF, 0x0406);
}
