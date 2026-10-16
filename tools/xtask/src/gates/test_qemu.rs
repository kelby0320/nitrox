use crate::*;

/// **The drive past 2 TiB `test-qemu` attaches** (Phase 6 Part G), in the build cache.
pub(crate) const PAST_TWO_TIB_IMAGE: &str = "test-qemu-usb-3tib.img";

/// The block `boot-probe` writes at: seven past the last a ten-byte command can name.
const PAST_TWO_TIB_BLOCK: u64 = (1 << 32) + 7;

/// What it writes there first; the rest of the sector is `(i * 31 + 7)`, as `boot-probe` makes it.
const PAST_TWO_TIB_MARK: &[u8; 32] = b"nitrox: past two tebibytes, G.5\n";

/// **The sector `boot-probe` wrote past 2 TiB, in the image** (Phase 6 Part G): where the guest
/// asked for it. A guest that read back what it wrote proves only that the write and the read
/// named the same block; a block number cut to 32 bits would name block 7 for both.
fn check_past_two_tib() -> R<()> {
    use std::os::unix::fs::FileExt;
    let big = build_cache().join(PAST_TWO_TIB_IMAGE);
    let mut got = [0u8; 512];
    let read = fs::File::open(&big).and_then(|f| f.read_exact_at(&mut got, PAST_TWO_TIB_BLOCK * 512));
    let low = fs::File::open(&big).and_then(|f| {
        let mut b = [0u8; 512];
        f.read_exact_at(&mut b, (PAST_TWO_TIB_BLOCK - (1 << 32)) * 512).map(|()| b)
    });
    let _ = fs::remove_file(&big);
    read.map_err(|e| format!("the 3 TiB image would not read: {e}"))?;
    let want: Vec<u8> =
        (0..512u32).map(|i| PAST_TWO_TIB_MARK.get(i as usize).copied().unwrap_or((i * 31 + 7) as u8)).collect();
    if got[..] != want[..] {
        let wrapped = low.is_ok_and(|b| b[..32] == PAST_TWO_TIB_MARK[..]);
        return Err(format!(
            "the sector boot-probe wrote at block {PAST_TWO_TIB_BLOCK} is not there in the image{}",
            if wrapped { ": it is at block 7, so the block number was cut to 32 bits" } else { "" }
        )
        .into());
    }
    println!("xtask: the sector boot-probe wrote past 2 TiB is in the 3 TiB image at block {PAST_TWO_TIB_BLOCK} ✓");
    Ok(())
}

/// **A stick as a shop sells one** (Phase 6 Part D): `mib` MiB, an MBR naming one FAT16 partition
/// from 1 MiB to the end, labelled `label`.
pub(crate) fn mbr_fat_stick(path: &Path, mib: u64, label: &str) -> R<()> {
    require_tool("mformat")?;
    let f = fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.set_len(mib * 1024 * 1024).map_err(|e| format!("size {}: {e}", path.display()))?;
    drop(f);
    let count = (mib * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0E)?;
    run(Command::new("mformat")
        .arg("-i")
        .arg(format!("{}@@1048576", path.display()))
        .arg("-T")
        .arg(count.to_string())
        .arg("-c")
        .arg("1")
        .arg("-v")
        .arg(label)
        .arg("::"))?;
    Ok(())
}

/// **Write an MBR into `img`'s first block** (Phase 6 Part D): one entry, of MBR type `kind`, for
/// `count` blocks from `first`, and the signature. The other three entries are left empty.
pub(crate) fn write_mbr(img: &Path, first: u32, count: u32, kind: u8) -> R<()> {
    write_mbr_parts(img, &[(first, count, kind)])
}

/// An MBR naming each of `parts` — first sector, sector count, type — in its own entry, up to four.
pub(crate) fn write_mbr_parts(img: &Path, parts: &[(u32, u32, u8)]) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut mbr = [0u8; 512];
    for (i, &(first, count, kind)) in parts.iter().take(4).enumerate() {
        let e = &mut mbr[0x1BE + i * 16..0x1CE + i * 16];
        e[4] = kind;
        e[8..12].copy_from_slice(&first.to_le_bytes());
        e[12..16].copy_from_slice(&count.to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    let mut f = fs::OpenOptions::new().write(true).open(img).map_err(|e| format!("open {}: {e}", img.display()))?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&mbr)?;
    Ok(())
}

/// Integration-test runner: build the `test-harness` image, boot it headless, and
/// adjudicate the run from QEMU's exit code. The guest ends the run by writing a
/// verdict to the `isa-debug-exit` device (init on success/failure, or the kernel
/// panic handler); QEMU then exits `(verdict << 1) | 1`. A hung boot is caught by a
/// wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`.
pub(crate) fn cmd_test_qemu(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    // Wall-clock ceiling: a hung boot must fail the run, not block CI forever. The
    // healthy self-test boot completes in a few seconds under TCG; 90 s is generous
    // (the demand-paging demo does many emulated-AHCI faults).
    const TIMEOUT_SECS: u32 = 90;
    // isa-debug-exit maps a guest port write `v` to host exit `(v << 1) | 1`: init's
    // PASS verdict (0x10) → 33; FAIL (0x11) → 35; the `timeout` tool uses 124.
    const PASS_EXIT: i32 = (0x10 << 1) | 1; // 33

    // **Spawned directly, not under `timeout(1)`.** The wrapper killed QEMU before we
    // could ask it anything, so a hang produced a bare "no verdict" line and nothing else —
    // which is exactly the ambiguity that made the `test-qemu` flake expensive to chase.
    // Owning the deadline means the guest is still alive at the moment it misses it.
    let qmp_sock = build_cache().join("test-qemu.qmp");
    fs::create_dir_all(build_cache())?;
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
    cmd.arg("-qmp").arg(format!("unix:{},server,nowait", qmp_sock.display()));
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        // The guest ends the run by writing its verdict to this port.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        // Headless: serial to our captured stdout, no display; `-smp 4` so the SMP
        // distribution/affinity self-tests are meaningful; never reboot on triple-fault.
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot");
    test_qemu_usb_args(&mut cmd)?;

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    println!("xtask: running integration tests under QEMU (timeout {TIMEOUT_SECS}s)…\n");
    let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;

    // Drain stdout on a thread: a guest that fills the pipe buffer would otherwise block
    // on its own serial writes and look exactly like the hang we are trying to diagnose.
    let stdout = child.stdout.take().ok_or("qemu stdout")?;
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut r = stdout;
        let mut buf = [0u8; 4096];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut g) = sink.lock() {
                g.extend_from_slice(&buf[..n]);
            }
        }
    });

    // **stderr gets its own drainer.** Piping it and never reading it loses exactly the
    // messages you need when a run fails for a *host* reason — an unavailable accelerator, a
    // bad image path, an option this QEMU does not know — leaving only
    // "FAILED (qemu exit 1)". Worse, an undrained pipe blocks QEMU once it fills, which
    // presents as a guest hang at the deadline: the precise misdiagnosis the rest of this
    // change exists to prevent (PR #177 review, finding 1).
    let stderr = child.stderr.take().ok_or("qemu stderr")?;
    let errs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let esink = errs.clone();
    let ereader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut r = stderr;
        let mut buf = [0u8; 4096];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Ok(mut g) = esink.lock() {
                g.extend_from_slice(&buf[..n]);
            }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(TIMEOUT_SECS as u64);
    let mut timed_out = false;
    let mut hot = HotPlug::default();
    let status = loop {
        match child.try_wait().map_err(|e| format!("wait qemu: {e}"))? {
            Some(st) => break st,
            None => {
                if let Ok(g) = captured.lock() {
                    hot.step(&g, &qmp_sock);
                }
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    // **Interrogate before killing.** This is the whole point of owning the
                    // deadline; once QEMU is dead the evidence is gone. The hot-plug's connection
                    // goes first: QEMU serves one QMP client at a time (PR #355 review).
                    hot.qmp = None;
                    match Qmp::connect(&qmp_sock) {
                        Ok(mut qmp) => dump_guest_state(&mut qmp),
                        Err(e) => println!("\nxtask: could not reach QMP to dump state: {e}"),
                    }
                    let _ = child.kill();
                    break child.wait().map_err(|e| format!("reap qemu: {e}"))?;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    let _ = reader.join();
    let _ = ereader.join();
    let _ = fs::remove_file(&qmp_sock);

    // Echo the captured serial log so the operator sees the boot + self-test output.
    if let Ok(g) = captured.lock() {
        std::io::stdout().write_all(&g)?;
    }
    if let Ok(g) = errs.lock()
        && !g.is_empty()
    {
        std::io::stderr().write_all(&g)?;
    }

    if timed_out {
        return Err(format!(
            "integration tests TIMED OUT after {TIMEOUT_SECS}s — no verdict (likely a hang); \
             see the guest state dumped above"
        )
        .into());
    }

    match status.code() {
        Some(code) if code == PASS_EXIT => {
            let transcript = captured.lock().map(|g| g.clone()).unwrap_or_default();
            check_login_chain(&transcript)?;
            check_demo_chain(&transcript)?;
            check_display_selftest(&transcript)?;
            check_ahci_msi_path(&transcript)?;
            check_block_read_selftest(&transcript)?;
            check_oversize_refused(&transcript)?;
            check_every_service_started(&transcript)?;
            check_servers_are_service_mgrs(&transcript)?;
            check_system_control(&transcript)?;
            check_hardware_facts(&transcript)?;
            check_hot_plug(&transcript, &hot)?;
            check_usb_listed(&transcript)?;
            check_past_two_tib()?;
            println!("\nxtask: integration tests PASSED (qemu exit {code})");
            Ok(())
        }
        Some(code) => Err(format!("integration tests FAILED (qemu exit {code}; expected {PASS_EXIT})").into()),
        None => Err("qemu terminated by a signal with no exit code".into()),
    }
}

/// Facts about the machine QEMU emulates that every boot now logs (Phase 5 Part D.1), and that
/// both boots below know the answer to: q35's ECAM window and its four MADT CPUs and one IOAPIC.
/// The framebuffer is not among them since Phase 5 Part E: `test-qemu` boots QEMU's default mode
/// and `check-report` the gates' size, so each states its own. Each fact is fragments that must all appear in **one**
/// line.
///
/// **Written down here, not derived** — a second statement of each answer, which is what makes it
/// an assertion. On the laptop these are the lines to compare (the plan's § What to compare on
/// the day).
pub(crate) const EMULATED_MACHINE_FACTS: &[&[&str]] = &[
    &["acpi: ECAM @0xe0000000 seg 0 bus 0-255"],
    &["madt: lapic uid 0 apic 0 enabled"],
    &["madt: lapic uid 1 apic 1 enabled"],
    &["madt: lapic uid 2 apic 2 enabled"],
    &["madt: lapic uid 3 apic 3 enabled"],
    &["madt: ioapic 0 @0xfec00000 gsi 0"],
    &["acpi: 1 IOAPIC, 5 src-override, 4 CPU; 1 ECAM region"],
    // **q35's FADT** (administration Part E.3): the reset register a reboot writes first — the
    // chipset's reset control at I/O 0xcf9, `RESET_REG_SUP` set in the flags — the RTC's century
    // register, and an 8042.
    &["fadt: flags 0x84a5"],
    &["fadt: reset register I/O 0xcf9 width 8, value 0xf"],
    &["fadt: century register CMOS 0x32"],
    &["fadt: boot arch 0x2; 8042 present"],
];

/// The facts `test-qemu`'s boot adds to [`EMULATED_MACHINE_FACTS`]: its disk is on AHCI, so the
/// controller is **claimed**, it has a UART, so COM1 is **present**, and an xHCI with devices.
///
/// **The ACPI tables are pinned by signature and OEM, not address or length.** Read off the
/// first run (2026-09-14): QEMU's FACP, APIC, HPET, MCFG and WAET, each `BOCHS BXPC`, plus a BGRT
/// that is OVMF's (`INTEL EDK2`, the boot logo) and so not QEMU's to promise. The addresses are
/// wherever this OVMF build put them, and CI's OVMF is another build.
const TEST_QEMU_FACTS: &[&[&str]] = &[
    &["acpi: FACP @", "oem BOCHS BXPC"],
    &["acpi: APIC @", "oem BOCHS BXPC"],
    &["acpi: HPET @", "oem BOCHS BXPC"],
    &["acpi: MCFG @", "oem BOCHS BXPC"],
    &["acpi: WAET @", "oem BOCHS BXPC"],
    &["madt: lapic-nmi uid 0xff lint 1 flags 0x0"],
    &["pci 00:1f.2 8086:2922 class 01.06.01 pin 1 caps msi64"],
    &["drivers: 00:1f.2 claimed by ahci, MSI vec "],
    // **The xHCI, claimed** (Phase 6 Part A.1), configured as the laptop's is: MSI, no MSI-X
    // ([`XHCI_DEVICE`]). Its facts are QEMU's, read off the first boot: the USB 3 ports first, then
    // the USB 2 ones, as its Supported Protocol capabilities say. And the rings, the doorbell and
    // the event ring proved by a No Op before the outcome is claimed. Its PCI line is held to what
    // QEMU 8.2 and 11 both say — the identity and a 64-bit MSI capability — since CI runs the first
    // and a workstation may run the second.
    &["pci 00:03.0 1033:0194 class 0c.03.30 ", "caps msi64"],
    &["xhci: 00:03.0 up: xHCI 1.0, 16 ports (USB 2: 9-16, USB 3: 1-8), 64 slots, 32-byte contexts, 0 scratchpad(s)"],
    &["xhci: 00:03.0: the command ring answers: a No Op completed"],
    &["drivers: 00:03.0 claimed by xhci, MSI vec "],
    // **What the hub thread's first round found** (Part A.2): each device by its port, IDs, class,
    // speed, name and match, without its serial, which QEMU makes from the PCI path. The stick
    // takes a USB 3 port; the rest, USB 2 ones. The smart-card reader is the one whose default
    // endpoint is not its speed's, so its enumeration evaluates it.
    &["xhci: port 3 at start: connected, SuperSpeed, enabled"],
    &["xhci: port 6 at start: connected, SuperSpeed, enabled"],
    &["xhci: 6 of 16 ports connected at start"],
    &["usb: port 3: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    &["usb: port 9: 0627:0001 class 03/01/01, high-speed, \"QEMU USB Keyboard (", "\": HID boot keyboard"],
    &["usb: port 10: 0627:0001 class 03/01/02, full-speed, \"QEMU USB Mouse (", "\": HID boot mouse"],
    &["usb: port 12: 0409:55aa class 09/00/00, full-speed, \"QEMU USB Hub (", "\": a hub, not supported"],
    &["usb: port 13: its default endpoint takes 64-byte packets, not 8; evaluated"],
    &["usb: port 13: 08e6:4433 class 0b/00/00, full-speed, \"QEMU USB CCID (", "\": nothing this kernel drives"],
    &["usb: port 6: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    &["usb: first round: 6 device(s) in "],
    // **The stick bound as a disk, its MBR read** (Phase 6 Part D): INQUIRY's strings and the
    // capacity QEMU gives a 16 MiB image, and the one FAT16 partition the gate wrote.
    &["usb: port 3: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 32768 blocks of 512 bytes, record "],
    &["mbr:  partition 1 lba 2048..32767 (30720 sectors) type 0x0e -> block node"],
    &["mbr: 1 partition(s)"],
    &["boot-probe: registry: usb disk blk-", ", 32768 blocks of 512, under usb-"],
    &["boot-probe: registry: usb disk blk-", "'s partition blk-", ", by mbr"],
    // **The drive past 2 TiB** (Phase 6 Part G): `READ CAPACITY(10)` cannot say its size, so it is
    // asked again with `READ CAPACITY(16)` and read with the sixteen-byte commands; published with
    // its size; and written past 2 TiB through its node, which `check_past_two_tib` finds on the
    // host.
    &["usb: port 6: LUN 0: 2 TiB or more: 6442450944 blocks, read with the sixteen-byte commands"],
    &["usb: port 6: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 6442450944 blocks of 512 bytes, record "],
    &["boot-probe: registry: usb disk blk-", ", 6442450944 blocks of 512, under usb-"],
    &["boot-probe: past 2 TiB: blk-", ", 6442450944 blocks: a sector written and read back at block 4294967303 ok"],
    // **A disk read again** (Phase 6 Part G): `boot-probe`'s `Reread` of the stick, which the
    // kernel rescans — its one partition departed and published again — and which the watch is
    // pinged for though nothing is mounted on it.
    &["usb: port 3: LUN 0: rescanned: 1 partition(s) departed, 1 published"],
    &["storage-service: blk-", " rescanned, as asked: 1 partition(s)"],
    &[
        "boot-probe: storage admin: a Reread of blk-",
        " rescanned it: its partition replaced, the old one's node refused after, and the watch pinged with no mount changed ok",
    ],
    // **Reported, with its clusters, as not served** (Phase 6 Part E.6): 512-byte clusters, which
    // `fs-server-fat` refuses, so the boot's mounts — what `boot-probe` checks — are as they were.
    &[
        "storage-service: blk-",
        " (partition partition 1 (unlabelled)): fat 'NXSTICK', left clean; not served: 512-byte clusters, smaller than a page",
    ],
    // **The disk the machine started from**: named by Limine's record, flagged in the registry,
    // and the only one flagged — the SATA disk the test image boots from (Phase 6 Part D.1).
    &["boot: the modules came from partition 1 of the GPT disk "],
    &["gpt: the disk the machine started from: its GUID is the one Limine loaded from"],
    &["boot-probe: registry: the boot disk is blk-0, and nothing else is flagged"],
    // **And the table got each** (Part A.3): `boot-probe`'s line per `UsbDevice` record, read
    // through `/dev/registry` — under the controller's function, with the IDs, class, port, speed
    // (as xHCI numbers them: 1 full, 3 high, 4 SuperSpeed) and name the hub thread logged. The
    // lines above say a device was enumerated; only these say the registry has it.
    &["boot-probe: registry: usb port 3, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    &["boot-probe: registry: usb port 9, 0627:0001 class 03/01/01, speed 3, under 00:03.0, \"QEMU USB Keyboard ("],
    &["boot-probe: registry: usb port 10, 0627:0001 class 03/01/02, speed 1, under 00:03.0, \"QEMU USB Mouse ("],
    &["boot-probe: registry: usb port 12, 0409:55aa class 09/00/00, speed 1, under 00:03.0, \"QEMU USB Hub ("],
    &["boot-probe: registry: usb port 13, 08e6:4433 class 0b/00/00, speed 1, under 00:03.0, \"QEMU USB CCID ("],
    &["boot-probe: registry: usb port 6, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    // **The keyboard and mouse bound** (Part B.2), each at the index after the i8042's two, and in
    // the registry under its device. That `input-server` was given them is `check_usb_listed`'s:
    // how many it was given depends on where the hot-plug lands.
    &["usb: port 9: keyboard at /dev/input/raw/2"],
    &["usb: port 10: mouse at /dev/input/raw/3"],
    &["boot-probe: registry: usb keyboard at /dev/input/raw/2, under usb-"],
    &["boot-probe: registry: usb mouse at /dev/input/raw/3, under usb-"],
    &["console: RX loopback self-test OK"],
    &["cpu: requires +x2apic +rdtscp +nx +smep +smap;"],
    // QEMU's default mode, which `test-qemu` keeps so that every CI run boots two sizes.
    &["framebuffer: 1280x800 pitch 5120 padding 0 bpp 32"],
    // **What a write to the screen costs, and why** (Phase 5 Part G's measurement). The values
    // are the machine's and are not asserted — what is asserted is that the boot *says* them,
    // since a report that quietly stopped carrying them is how the laptop's next boot would
    // measure nothing. QEMU's own framebuffer is uncacheable here too, which is worth knowing:
    // the emulator disagrees with the hardware about the *cost*, not about the configuration.
    &["cache policy: ", "variable range(s), default "],
    &["cache policy: page attributes 0:"],
    // **The bootloader is held to the table the kernel programs** (Phase 5 Part G.1). The kernel
    // owns it either way; what this asserts is that nothing in flight — the console's mapping,
    // every `kvmap` MMIO mapping — chose its entry out of a different one. A bootloader that
    // changes its table fails here rather than silently changing what a live mapping means.
    &["cache policy: the kernel's table is installed; the bootloader's was the same"],
    // **What each block device says it is** (Phase 5 Part H.1). The installer refuses to write
    // anything that is not a `disk`, and this registry holds whole disks, the partitions found on
    // them and memory published as a disk — so a partition that called itself a disk would be a
    // partition table written over a filesystem. The capacity and the name are asserted with it:
    // they are what a person confirms a destructive operation by.
    // **By what it is, not where it lands**: a test image's scratch RAM disk (administration Part
    // C.5b) is published before any partition, so the ESP is `/dev/blk/2` there and would be
    // `/dev/blk/1` without it. The disk is first in any image.
    &["test-harness: /dev/blk/0 is a disk, 128 MiB, named QEMU HARDDISK"],
    &["test-harness: /dev/blk/", " is a partition, 48 MiB, named NITROX_ESP"],
    &["framebuffer: ", " is ", "memory, and a plain mapping of it is write-back"],
    // **What each mapping asks for**, which is what the timings are evidence of — and the half
    // QEMU agrees with the hardware about. Both ask for write-combining since Part G; before it,
    // the bootloader's did and a mapping this kernel made asked for write-back, which an
    // uncacheable range overrode.

    // **The two mappings agree since Part G.3**, which is the fix in one line: the aperture's
    // object says write-combining, so every mapping of it asks for that, the bootloader's included.
    &["framebuffer: the console's mapping asks for write-combining"],
    &["framebuffer: a mapping made as userspace's is asks for write-combining"],
    // **And what the client actually got.** The two lines above are the kernel measuring its own
    // mappings; this one is the object `/dev/framebuffer` mints, which is the only place the
    // aperture's answer reaches userspace and the only fact a compositor's speed depends on.
    &["framebuffer: handed out as write-combining memory"],
    &["framebuffer: a full-screen fill of ", "KiB took ", "through the console's mapping"],
    // The pair is the point: same loop, same pixels, two page-table entries. A boot that lost the
    // second line would leave the comparison looking like one number nobody could interpret.
    &["framebuffer: the same fill took ", "through a mapping made as userspace's is"],
];

/// The facts in `wanted` that no single line of `lines` carries every fragment of, each joined
/// for a message. Empty when every fact is there.
pub(crate) fn missing_facts<'a>(lines: &[&str], wanted: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<String> {
    wanted
        .into_iter()
        .filter(|fact| !lines.iter().any(|line| fact.iter().all(|frag| line.contains(frag))))
        .map(|fact| fact.join(" … "))
        .collect()
}

/// Assert what the boot says about the machine (Phase 5 Part D.4): [`EMULATED_MACHINE_FACTS`],
/// [`TEST_QEMU_FACTS`], and the bootloader — this build's pinned Limine, on 64-bit UEFI.
fn check_hardware_facts(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    let lines: Vec<&str> = text.lines().collect();
    let limine = format!("boot: Limine {} on UEFI (64-bit), base revision 6", LIMINE_VERSION.trim_start_matches('v'));
    let bootloader: &[&str] = &[&limine];
    // **The build it is** (the laptop polish's Part D): the commit this process built from.
    let built_from = built_from_line();
    let commit: &[&str] = &[&built_from];
    let missing = missing_facts(
        &lines,
        EMULATED_MACHINE_FACTS.iter().chain(TEST_QEMU_FACTS).copied().chain([bootloader, commit]),
    );
    if !missing.is_empty() {
        return Err(format!(
            "the boot did not report what QEMU's machine is — no line says: {missing:?}. Each is \
             one line of the hardware report every boot logs (kernel_main's handoff, the ACPI \
             walk, PCI enumeration, the driver outcomes, the framebuffer record)"
        )
        .into());
    }
    println!(
        "xtask: the boot reported QEMU's machine as it is — tables, CPUs, IOAPIC, framebuffer, a claimed AHCI and xHCI, COM1 ✓"
    );
    Ok(())
}

/// Assert that the block driver **refused** a transfer larger than one command can describe.
///
/// **The same hole `check_display_selftest` documents, met again.** `drivers::self_test` prints
/// a verdict and does not call `SYS_TEST_EXIT`, so with the guard in `dispatch_block_irp`
/// deleted the guest printed `ahci: oversize self-test FAIL (249 frags accepted, table holds
/// 248)` and `test-qemu` exited **0**. A self-test whose failure nobody reads is not a test —
/// which is precisely what the comment on `check_display_selftest` had already worked out, one
/// declaration over.
///
/// Requiring the `OK` line rather than merely rejecting `FAIL` is deliberate: it also catches
/// the check not running at all, which is what a refactor that dropped the call would produce.
///
/// **The skip arm names every cause, because an earlier version named one and it was the
/// unlikeliest.** It said the check is skipped "only when the first block device publishes no
/// fragment limit" — but `oversize_refused` was then the *last* statement of
/// `drivers::self_test`, so a regression in the sector-0 read path skipped it too, and this
/// message would have sent the author to `max_frags` for a fault three functions away
/// (PR #293 review, 2). The call has moved above those returns, and
/// [`check_block_read_selftest`] adjudicates the read separately — so a read regression now
/// fails on its own check, with its own message, before this one is consulted.
fn check_oversize_refused(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: oversize self-test OK") {
        println!("xtask: an oversized block transfer was refused ✓");
        return Ok(());
    }
    if text.contains("ahci: oversize self-test FAIL") {
        return Err("a block transfer needing more fragments than the device's command table \
             holds was NOT refused — the driver would write PRDT entries past the table it \
             owns, and the controller would DMA to whatever followed. See \
             `dispatch_block_irp`'s `max_frags` check and `TODO(block-transfer-split)`."
            .into());
    }
    Err("the oversize block self-test did not run — expected \
         \"ahci: oversize self-test OK\" in the transcript. It is skipped when no block device \
         was discovered at all, when the first one publishes no fragment limit (the ramdisk's \
         `u32::MAX`), or when the probe's own allocation failed. An AHCI disk that came up is \
         none of those."
        .into())
}

/// Assert that the AHCI driver acquired its interrupt over **MSI**, not by falling back to
/// INTx.
///
/// **A passing `test-qemu` proves nothing about this on its own**, which is the whole reason
/// the check exists. QEMU's firmware programs the PCI interrupt-line register to match the
/// IOAPIC, so the INTx fallback works perfectly here — the boot, the block read self-test and
/// the verdict are identical either way. Real UEFI frequently leaves that register
/// meaningless, so a silent fallback is a bug only the target machine would report, by not
/// booting.
///
/// **Matches the path token and never the vector**, deliberately. The vector is not stable
/// across builds: the selftest image's `IrqRouter::self_test` registers `pit_tick` and never
/// releases the slot, so AHCI lands on `0x31` there and `0x30` in a release image, and every
/// captured transcript in `tools/build-cache` splits on exactly that line. A matcher written
/// against the literal would pass this gate — `test-qemu` is a selftest image — and fail the
/// first time it were pointed at `check-login` or `test-interactive`. Part A's own switch
/// moved the number again.
///
/// The INTx arm is a **failure, not a skip**: every AHCI controller on either target machine
/// advertises MSI (capability at `0x80` on both), so taking the fallback under QEMU means the
/// capability walk, the message composition or the capability write went wrong — not that the
/// hardware lacked the feature.
fn check_ahci_msi_path(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: irq via MSI") {
        println!("xtask: the AHCI controller took the MSI interrupt path ✓");
        return Ok(());
    }
    if text.contains("ahci: irq via INTx") {
        return Err("the AHCI driver fell back to INTx. QEMU's ICH9 AHCI advertises MSI at \
             capability 0x80, so the fallback firing here means the driver did not find it or \
             could not program it — `pci::find_capability`, `pci::read_msi`, \
             `ArchIrqInstall::install_msi` (which declines a destination wider than eight \
             bits), or `pci::Config::map`, whose failure logs its own line just above. The \
             boot still passes, because QEMU's firmware programs the interrupt-line register \
             and real UEFI often does not — which is the bug this gate exists to catch before \
             the laptop does."
            .into());
    }
    Err("the AHCI driver printed no interrupt-path line — expected \"ahci: irq via MSI\" or \
         \"ahci: irq via INTx\". Both arms print one, so its absence means the controller \
         never reached interrupt installation at all: no ABAR, no SATA disk on an implemented \
         port, a failed DMA allocation, or an HBA declined for lacking CAP.S64A. Each of those \
         logs its own line above this point."
        .into())
}

/// Assert that the block driver's **sector-0 read** self-test passed.
///
/// **The hole this branch is about, closed for its neighbour too.** `drivers::self_test` prints
/// a verdict and never calls `SYS_TEST_EXIT`, so before this every outcome of the real AHCI read
/// path — a dispatch error, a non-zero status, a short transfer — printed `ahci: read self-test
/// FAIL …` and `test-qemu` exited 0. `grep "read self-test" tools/xtask/src/main.rs` returned
/// nothing (PR #293 review, 2).
///
/// **Accepts either success line.** `read self-test OK` is the read plus a `0x55AA` boot
/// signature; `read self-test: sector 0 read OK … but sig` is the read succeeding on an image
/// whose sector 0 carries something else. Only the first appears today (the image has a
/// protective MBR), but requiring it would fail this gate for a change to the *image* rather
/// than to the driver, and what this checks is the driver.
fn check_block_read_selftest(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if text.contains("ahci: read self-test FAIL") {
        return Err("the AHCI sector-0 read self-test FAILED — the real block read path \
             (dispatch → controller DMA → completion) did not deliver. The transcript line \
             says which half: `(dispatch err …)` is a refusal to start, `(status … result …)` \
             is a completed request that errored or came up short."
            .into());
    }
    if text.contains("ahci: read self-test OK") || text.contains("sector 0 read OK") {
        println!("xtask: the block read self-test passed ✓");
        return Ok(());
    }
    Err("the AHCI read self-test did not run — expected an \"ahci: read self-test\" line in \
         the transcript. It is skipped when no block device was discovered, or when the probe \
         could not allocate its buffer."
        .into())
}

/// Assert that the display self-test **passed**.
///
/// **The same hole `check_demo_chain` closes, one declaration over.** `init` adjudicated this
/// exit code and fired `test_exit(false)` on it; retrofit Part C2 made the self-test a
/// declaration with `policy = "never"`, so `service-mgr` logs the code and does nothing with
/// it, and `display-selftest` never calls `SYS_TEST_EXIT` itself. Forcing its hash comparison
/// to fail produces `display-selftest: FAILED`, `exited code=1`, and `integration tests
/// PASSED` (PR #229 review, finding 1).
///
/// Exit code **2** is the one the deleted comment singled out: it means the framebuffer
/// binding was missing, and folding that into success "is how the entire display arm could go
/// missing with `test-qemu` still green, which is exactly what an earlier version of this code
/// did". Requiring the `PASSED` line rejects 1 and 2 alike, and rejects the self-test not
/// running at all.
///
/// `check-display` is not a substitute: it is a separate, path-filtered workflow, and it does
/// not read this exit code.
fn check_display_selftest(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("display-selftest: PASSED") {
        return Err("the display self-test did not pass — expected \
             \"display-selftest: PASSED\" in the transcript. \"FAILED\" is a hash \
             mismatch; no line at all (exit code 2) means it could not bind the framebuffer, \
             which is the display arm going missing rather than a rendering bug."
            .into());
    }
    println!("xtask: the display self-test passed ✓");
    Ok(())
}

/// Assert that **every declared service actually started**.
///
/// `init` used to fire `test_exit(false)` when a spawn failed; `spawn_service` logs and carries
/// on, so a `ui-testclient` that never starts reaches PASS. This is deliberately a check on the
/// *failure* lines rather than a list of expected services: a new declaration is covered the
/// day it is added, and there is nothing to keep in step (PR #229 review, finding 1).
fn check_every_service_started(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    // And, since `service-mgr` starts the servers (administration Part E.1), each way one fails to
    // come up: no `Ready`, a refusal, or a path it could not be bound at.
    for pat in [
        "service-mgr: image not found",
        "service-mgr: spawn FAIL",
        "did not come up",
        "sent no Ready within",
        "service-mgr: bind FAIL at",
        "registry bind FAIL",
        "service-mgr: no route for",
        "-- sessions will not reach it",
        // A declaration the parser could not take, a restart that could not spawn, and a file
        // that lost its critical servers (PR #340 review, findings 2 and 3).
        "' skipped: ",
        "' could not be restarted",
        "-- starting nothing",
    ] {
        if let Some(i) = text.find(pat) {
            let line: String = text[i..].lines().next().unwrap_or(pat).into();
            return Err(format!(
                "a declared service failed to start, and nothing in the guest fails the run \
                 for it: {line:?}"
            )
            .into());
        }
    }
    println!("xtask: every declared service started ✓");
    Ok(())
}

/// Assert that the demo chain **finished**, not merely that it ran.
///
/// **This replaces a check `init` used to make.** `init::supervise` ran the chain
/// synchronously and failed the run on a non-zero exit; retrofit Part C2 made it a service
/// declaration, so nothing in the guest reads its exit code any more — `policy = "never"`
/// means service-mgr starts it once and does not react.
///
/// Without this, a chain that dies partway reaches PASS: the checks that follow it are
/// `boot-probe`'s, and `boot-probe` waits for the chain to *exit*, not to succeed. That is not
/// hypothetical — it happened on the first C2 boot. The chain stopped at
/// `test-harness: session user bind FAIL`, because `init` had spawned it with
/// `BIND_NAMESPACE` and the declaration could not yet say so, and `test-qemu` passed anyway
/// with half the spawns missing.
fn check_demo_chain(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("test-harness: all smoke tests passed") {
        return Err("the demo chain did not finish — expected \
             \"test-harness: all smoke tests passed\" in the transcript. The last \
             \"test-harness:\" line names the stage it stopped at; a `FAIL` there is the \
             real fault. `boot-probe` waits for the chain to exit, not to succeed, so the \
             run reaches PASS either way and only this says otherwise."
            .into());
    }
    println!("xtask: the demo chain finished (all smoke tests passed) ✓");
    Ok(())
}

/// The servers `service-mgr` starts, and the root path each is bound at (administration Part
/// E.1). **Written down a second time here**, from `SERVICES_TOML`, so the gate does not take its
/// aim from the declarations it checks.
const SERVICE_MGR_SERVERS: &[(&str, &str)] = &[
    ("auth-service", "/svc/auth"),
    ("logging-service", "/log"),
    ("tty-server", "/dev/tty"),
    ("clipboard-server", "/dev/clipboard"),
    ("view-broker", "/svc/views"),
    ("device-mgr", "/svc/devices"),
    ("storage-service", "/svc/storage"),
    ("input-server", "/dev/input/new"),
    ("compositor", "/dev/draw"),
];

/// **`init` holds the system-control object** (administration Part E.3): the kernel made it and
/// handed it over in `rdx`, and `init` found a `SystemControl` with `WRITE` there. The other half,
/// that nothing else can use `sys_power`, is `boot-probe`'s, which gates the verdict itself.
fn check_system_control(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("init: holds the system-control object") {
        return Err("`init: holds the system-control object` is not in the transcript: the kernel's \
                    handoff in `rdx` did not reach init as a SystemControl with WRITE"
            .into());
    }
    println!("xtask: init holds the system-control object ✓");
    Ok(())
}

/// **`service-mgr` bound every server, and `init` bound none of them** (administration Part E.1).
/// `init` starts only its mounts, the profile server at `/bin`, and `service-mgr`; a line of
/// `init`'s naming one of these servers' paths would be the old arrangement back.
fn check_servers_are_service_mgrs(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    for (name, path) in SERVICE_MGR_SERVERS {
        let line = format!("service-mgr: {name} bound at {path}");
        if !text.contains(&line) {
            return Err(format!("`{line}` is not in the transcript: service-mgr did not bind {name}").into());
        }
        if text.lines().any(|l| l.starts_with("init: ") && l.contains(name)) {
            return Err(
                format!("init's log names {name}, which service-mgr starts since administration Part E.1").into()
            );
        }
    }
    // **And the endpoints two of them mint for sessions** (Part E.1b), each behind a route of its
    // own: what the login supervisors are handed for `/dev/devices` and `/storage`.
    for (name, through) in [("device-mgr", "info-endpoint"), ("storage-service", "session-endpoint")] {
        let line = format!("service-mgr: sessions reach {name} through its {through}");
        if !text.contains(&line) {
            return Err(format!("`{line}` is not in the transcript: no route to it for sessions").into());
        }
    }
    println!("xtask: service-mgr bound all {} servers, and init none of them ✓", SERVICE_MGR_SERVERS.len());
    Ok(())
}

/// Assert that the login chain came up — `session-mgr` holding the endpoints it needs to
/// build a session.
///
/// **This replaces a verdict `session-mgr` used to fire itself.** It called `verdict(false)`
/// when its endpoint handoff failed, which is a session supervisor adjudicating a test run;
/// removing that (retrofit Part B) would otherwise have let a broken login chain reach PASS,
/// because nothing else in `test-qemu` reads it. The transcript does.
///
/// It does **not** assert that anyone logged in: nothing types a password here, and after
/// Part B nothing auto-logs-in either. Whether a real login works is
/// `cargo xtask test-interactive`'s question, on the release image.
///
/// **It depends on an ordering that is not causal, which is worth naming rather than
/// discovering.** `service-mgr` queues session-mgr's handoffs before it starts any declaration
/// after the last server, so the *send* is ordered — but session-mgr still has to be scheduled
/// to print this line before `boot-probe`, spawned afterwards, fires PASS and terminates the
/// machine. If that ever inverts, a healthy boot fails here, which is the expensive kind of
/// red.
///
/// The margin is large and was measured rather than assumed. It was six lines before
/// administration Part E.1a; on E.1a's first passing boot session-mgr's line landed **252
/// lines** before `service-mgr: starting service 'boot-probe'`, with the display self-test,
/// `nxterm` and the graphical test clients started between. If this ever goes red on a boot that looks
/// healthy, check that ordering first, and consider asserting on
/// `service-mgr: login chain up` instead: that line *is* causally before `boot-probe`, at the
/// cost of proving only that the handoffs were sent, not that they arrived.
fn check_login_chain(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("session-mgr: received fs + profile endpoints; auth resolved from /svc/auth") {
        return Err("the login chain did not come up — session-mgr never reported its \
             endpoints (look for \"session-mgr: endpoint handoff FAIL\", or for \
             service-mgr failing to spawn it at all)"
            .into());
    }
    println!("xtask: the login chain came up (session-mgr holds its endpoints) ✓");
    Ok(())
}
