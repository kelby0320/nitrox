//! `test-qemu`: the headless self-test image, and the facts its transcript must hold.

use crate::*;

pub(crate) const PAST_TWO_TIB_IMAGE: &str = "test-qemu-usb-3tib.img";

/// The block `boot-probe` writes at: seven past the last a ten-byte command can name.
const PAST_TWO_TIB_BLOCK: u64 = (1 << 32) + 7;

/// What it writes there first; the rest of the sector is `(i * 31 + 7)`, as `boot-probe` makes it.
const PAST_TWO_TIB_MARK: &[u8; 32] = b"nitrox: past two tebibytes, G.5\n";

/// The sector `boot-probe` wrote past 2 TiB, in the image: a guest reading back what it wrote
/// cannot tell a block number cut to 32 bits (block 7).
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

pub(crate) fn write_mbr(img: &Path, first: u32, count: u32, kind: u8) -> R<()> {
    write_mbr_parts(img, &[(first, count, kind)])
}

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

/// Integration-test runner: boot the `test-harness` image headless and adjudicate the run from
/// QEMU's exit code or a timeout. See `docs/conventions/qemu-integration-tests.md`.
pub(crate) fn cmd_test_qemu(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    // A hung boot fails the run; 90 s is generous for the demand-paging demo's emulated-AHCI
    // faults.
    const TIMEOUT_SECS: u32 = 90;
    // isa-debug-exit maps a port write `v` to exit `(v << 1) | 1`: PASS (0x10) is 33, FAIL (0x11)
    // 35; `timeout` uses 124.
    const PASS_EXIT: i32 = (0x10 << 1) | 1; // 33

    // Spawned directly, not under `timeout(1)`, which killed QEMU before it could be asked
    // anything: owning the deadline keeps the guest alive when it is missed.
    let qmp_sock = build_cache().join("test-qemu.qmp");
    fs::create_dir_all(build_cache())?;
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
    cmd.arg("-qmp").arg(format!("unix:{},server,nowait", qmp_sock.display()));
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        // Four CPUs, so the SMP placement self-tests mean something.
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot");
    test_qemu_usb_args(&mut cmd)?;

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    println!("xtask: running integration tests under QEMU (timeout {TIMEOUT_SECS}s)…\n");
    let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;

    let stdout = child.stdout.take().ok_or("qemu stdout")?;
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = captured.clone();
    // Drained on a thread: a full pipe blocks the guest's serial writes, which looks like a hang.
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

    // stderr gets its own drainer: unread, a host failure's messages are lost and a full pipe
    // blocks QEMU, which looks like a guest hang.
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
                    // Interrogate before killing: once QEMU is dead the evidence is gone. The
                    // hot-plug's connection goes first, since QEMU serves one QMP client at a time.
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
            check_reclaim_selftest(&transcript)?;
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

/// Facts about the machine QEMU emulates that every boot logs: q35's ECAM window, four MADT CPUs
/// and one IOAPIC. The framebuffer is not among them (`test-qemu` boots QEMU's default mode,
/// `check-report` the gates' size). Each fact is fragments that must all appear in one line,
/// written down rather than derived.
pub(crate) const EMULATED_MACHINE_FACTS: &[&[&str]] = &[
    &["acpi: ECAM @0xe0000000 seg 0 bus 0-255"],
    &["madt: lapic uid 0 apic 0 enabled"],
    &["madt: lapic uid 1 apic 1 enabled"],
    &["madt: lapic uid 2 apic 2 enabled"],
    &["madt: lapic uid 3 apic 3 enabled"],
    &["madt: ioapic 0 @0xfec00000 gsi 0"],
    &["acpi: 1 IOAPIC, 5 src-override, 4 CPU; 1 ECAM region"],
    // q35's FADT: the reset register a reboot writes first, `RESET_REG_SUP` in the flags.
    &["fadt: flags 0x84a5"],
    &["fadt: reset register I/O 0xcf9 width 8, value 0xf"],
    &["fadt: century register CMOS 0x32"],
    &["fadt: boot arch 0x2; 8042 present"],
];

/// The facts `test-qemu`'s boot adds: AHCI claimed, COM1 present, an xHCI with devices. ACPI tables
/// are pinned by signature and OEM, not address or length (the BGRT is OVMF's, and addresses vary
/// by OVMF build).
const TEST_QEMU_FACTS: &[&[&str]] = &[
    &["acpi: FACP @", "oem BOCHS BXPC"],
    &["acpi: APIC @", "oem BOCHS BXPC"],
    &["acpi: HPET @", "oem BOCHS BXPC"],
    &["acpi: MCFG @", "oem BOCHS BXPC"],
    &["acpi: WAET @", "oem BOCHS BXPC"],
    &["madt: lapic-nmi uid 0xff lint 1 flags 0x0"],
    &["pci 00:1f.2 8086:2922 class 01.06.01 pin 1 caps msi64"],
    &["drivers: 00:1f.2 claimed by ahci, MSI vec "],
    // The xHCI, claimed, configured as the laptop's is (MSI, no MSI-X: [`XHCI_DEVICE`]). Its PCI
    // line is held to what QEMU 8.2 and 11 both say: CI runs the first, a workstation the second.
    &["pci 00:03.0 1033:0194 class 0c.03.30 ", "caps msi64"],
    &["xhci: 00:03.0 up: xHCI 1.0, 16 ports (USB 2: 9-16, USB 3: 1-8), 64 slots, 32-byte contexts, 0 scratchpad(s)"],
    &["xhci: 00:03.0: the command ring answers: a No Op completed"],
    &["drivers: 00:03.0 claimed by xhci, MSI vec "],
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
    &["usb: port 3: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 32768 blocks of 512 bytes, record "],
    &["mbr:  partition 1 lba 2048..32767 (30720 sectors) type 0x0e -> block node"],
    &["mbr: 1 partition(s)"],
    &["boot-probe: registry: usb disk blk-", ", 32768 blocks of 512, under usb-"],
    &["boot-probe: registry: usb disk blk-", "'s partition blk-", ", by mbr"],
    &["usb: port 6: LUN 0: 2 TiB or more: 6442450944 blocks, read with the sixteen-byte commands"],
    &["usb: port 6: LUN 0: disk \"QEMU QEMU HARDDISK (", "\", 6442450944 blocks of 512 bytes, record "],
    &["boot-probe: registry: usb disk blk-", ", 6442450944 blocks of 512, under usb-"],
    &["boot-probe: past 2 TiB: blk-", ", 6442450944 blocks: a sector written and read back at block 4294967303 ok"],
    &["usb: port 3: LUN 0: rescanned: 1 partition(s) departed, 1 published"],
    &["storage-service: blk-", " rescanned, as asked: 1 partition(s)"],
    &[
        "boot-probe: storage admin: a Reread of blk-",
        " rescanned it: its partition replaced, the old one's node refused after, and the watch pinged with no mount changed ok",
    ],
    &[
        "storage-service: blk-",
        " (partition partition 1 (unlabelled)): fat 'NXSTICK', left clean; not served: 512-byte clusters, smaller than a page",
    ],
    &["boot: the modules came from partition 1 of the GPT disk "],
    &["gpt: the disk the machine started from: its GUID is the one Limine loaded from"],
    &["boot-probe: registry: the boot disk is blk-0, and nothing else is flagged"],
    &["boot-probe: registry: usb port 3, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    &["boot-probe: registry: usb port 9, 0627:0001 class 03/01/01, speed 3, under 00:03.0, \"QEMU USB Keyboard ("],
    &["boot-probe: registry: usb port 10, 0627:0001 class 03/01/02, speed 1, under 00:03.0, \"QEMU USB Mouse ("],
    &["boot-probe: registry: usb port 12, 0409:55aa class 09/00/00, speed 1, under 00:03.0, \"QEMU USB Hub ("],
    &["boot-probe: registry: usb port 13, 08e6:4433 class 0b/00/00, speed 1, under 00:03.0, \"QEMU USB CCID ("],
    &["boot-probe: registry: usb port 6, 46f4:0001 class 08/06/50, speed 4, under 00:03.0, \"QEMU USB HARDDRIVE ("],
    &["usb: port 9: keyboard at /dev/input/raw/2"],
    &["usb: port 10: mouse at /dev/input/raw/3"],
    &["boot-probe: registry: usb keyboard at /dev/input/raw/2, under usb-"],
    &["boot-probe: registry: usb mouse at /dev/input/raw/3, under usb-"],
    &["console: RX loopback self-test OK"],
    &["cpu: requires +x2apic +rdtscp +nx +smep +smap;"],
    // QEMU's default mode, which `test-qemu` keeps so that every CI run boots two sizes.
    &["framebuffer: 1280x800 pitch 5120 padding 0 bpp 32"],
    // The screen's cost values are not asserted, only that the boot says them, or a report that
    // stopped carrying them would leave the laptop's next boot measuring nothing.
    &["cache policy: ", "variable range(s), default "],
    &["cache policy: page attributes 0:"],
    &["cache policy: the kernel's table is installed; the bootloader's was the same"],
    // What each block device says it is: the installer refuses anything not a `disk`, so a
    // partition calling itself one would be a partition table written over a filesystem. The ESP
    // is matched by kind, not index, as a test image's scratch RAM disk is published first.
    &["test-harness: /dev/blk/0 is a disk, 128 MiB, named QEMU HARDDISK"],
    &["test-harness: /dev/blk/", " is a partition, 48 MiB, named NITROX_ESP"],
    &["framebuffer: ", " is ", "memory, and a plain mapping of it is write-back"],
    // What each mapping asks for, the half QEMU agrees with the hardware about: write-combining.
    &["framebuffer: the console's mapping asks for write-combining"],
    &["framebuffer: a mapping made as userspace's is asks for write-combining"],
    &["framebuffer: handed out as write-combining memory"],
    &["framebuffer: a full-screen fill of ", "KiB took ", "through the console's mapping"],
    &["framebuffer: the same fill took ", "through a mapping made as userspace's is"],
];

pub(crate) fn missing_facts<'a>(lines: &[&str], wanted: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<String> {
    wanted
        .into_iter()
        .filter(|fact| !lines.iter().any(|line| fact.iter().all(|frag| line.contains(frag))))
        .map(|fact| fact.join(" … "))
        .collect()
}

fn check_hardware_facts(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    let lines: Vec<&str> = text.lines().collect();
    let limine = format!("boot: Limine {} on UEFI (64-bit), base revision 6", LIMINE_VERSION.trim_start_matches('v'));
    let bootloader: &[&str] = &[&limine];
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

/// Assert that the block driver **refused** a transfer larger than one command can describe
/// (`drivers::self_test` does not call `SYS_TEST_EXIT`, so require the `OK` line).
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

/// Assert that the AHCI driver took its interrupt over **MSI**, not INTx: QEMU's firmware programs
/// the interrupt-line register so INTx works and the boot is identical. It matches the path token,
/// never the vector, which varies by build.
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

/// Assert that the block driver's sector-0 read self-test passed, by either success line: the
/// second is the read on an image whose sector 0 holds no boot signature, an image's change.
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

/// Assert that the kernel's reclaim-under-load self-test ran: a boot that skipped it passes.
fn check_reclaim_selftest(transcript: &[u8]) -> R<()> {
    if String::from_utf8_lossy(transcript).contains("io: reclaim-under-load self-test OK") {
        println!("xtask: completed IRPs stayed bounded with every CPU busy ✓");
        return Ok(());
    }
    Err("the kernel's reclaim-under-load self-test printed no OK line: it runs in every \
         test-harness boot, after the APs come up, and a failure panics with its reason"
        .into())
}

/// Assert that the display self-test **passed**: it has `policy = "never"` and never calls
/// `SYS_TEST_EXIT`, so a failure only appears as a missing `PASSED` line.
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

/// Assert that every declared service started, by the failure lines rather than a list of
/// services, so a declaration is covered the day it is added.
fn check_every_service_started(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    for pat in [
        "service-mgr: image not found",
        "service-mgr: spawn FAIL",
        "did not come up",
        "sent no Ready within",
        "service-mgr: bind FAIL at",
        "registry bind FAIL",
        "service-mgr: no route for",
        "-- sessions will not reach it",
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

/// Assert that the demo chain **finished**, not merely that it ran: `boot-probe` waits for it to
/// exit, not to succeed, so a chain that dies partway otherwise reaches PASS.
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

/// The servers `service-mgr` starts and the root path each is bound at, written down a second time
/// from `SERVICES_TOML` so the gate does not take its aim from the declarations it checks.
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

/// `init` holds the system-control object; that nothing else can use `sys_power` is `boot-probe`'s.
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

/// `service-mgr` bound every server and `init` none: an `init` line naming one is the old way back.
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
    for (name, through) in [("device-mgr", "info-endpoint"), ("storage-service", "session-endpoint")] {
        let line = format!("service-mgr: sessions reach {name} through its {through}");
        if !text.contains(&line) {
            return Err(format!("`{line}` is not in the transcript: no route to it for sessions").into());
        }
    }
    println!("xtask: service-mgr bound all {} servers, and init none of them ✓", SERVICE_MGR_SERVERS.len());
    Ok(())
}

/// Assert that the login chain came up — `session-mgr` holding the endpoints it needs to build a
/// session.
///
/// Its order against `boot-probe`'s PASS is not causal (the margin was 252 lines): if a healthy
/// boot fails here, check that first, or assert on `service-mgr: login chain up`.
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
