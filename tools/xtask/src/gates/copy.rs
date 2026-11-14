//! `check-copy`: a file twice the machine's memory copied on a stick, checked by the host.

use crate::*;

const COPY_LABEL: &str = "nxcopy";
const BIG: &str = "big";
const BIG_COPY: &str = "big-copy";

/// The source: twice the 256 MiB the gates give a machine.
const BIG_MIB: u64 = 512;

/// The stick: room for the source and its copy, and the filesystem's own tables.
const STICK_MIB: u64 = 1280;

/// **Where the source holds bytes**, a page each, among holes: either side of a 16 MiB window's
/// edge, and the last page. Each page is its offset's own pattern, so a window copied to the wrong
/// place, or not at all, shows.
const MARKS: [u64; 4] = [0, (16 << 20) - 4096, 16 << 20, (BIG_MIB << 20) - 4096];

fn mark_byte(page_at: u64, i: usize) -> u8 {
    ((page_at >> 12) as u8).wrapping_mul(31).wrapping_add(i as u8)
}

/// **A file twice the machine's memory, copied on a stick.** The page cache keeps what it holds
/// until its object goes, so the copy finishes only if each window's pages are let go once it is
/// copied (`IO_NOCACHE`); without that it runs out of memory a quarter of the way in.
/// The test live image boots beside a stick holding a sparse 512 MiB file with a page of bytes at
/// each of [`MARKS`]; `copy` copies it, `disk --eject` unmounts the stick, and the host checks the
/// filesystem clean and the copy whole: its size, and every mark where the source had it.
pub(crate) fn cmd_check_copy(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    require_tool("mke2fs")?;
    require_tool("debugfs")?;
    require_tool("e2fsck")?;
    cmd_image_live_for(BuildMode::Selftest)?;
    let work = build_cache().join("check-copy");
    fs::create_dir_all(&work)?;
    let stick = work.join("stick.img");
    copy_stick(&stick, &work)?;

    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=boot,format=raw,file={}", live_test_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=boot")
        .arg("-drive")
        .arg(format!("if=none,id=copy,format=raw,file={}", stick.display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=copy,removable=true")
        .arg("-display")
        .arg("none")
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    println!("xtask: copy gate — a {BIG_MIB} MiB file copied on a stick, in a machine of 256 MiB…\n");
    let mut session = Session::spawn(cmd, "check-copy")?;
    let result = run_copy_steps(&mut session);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    println!("\nxtask: the machine is stopped; the stick, on the host:");
    check_copy_stick(&stick)?;
    println!("\nxtask: a file twice the machine's memory was copied whole, a window at a time ✓");
    Ok(())
}

/// The stick: a whole-disk ext4 labelled [`COPY_LABEL`] holding [`BIG`], sparse but for [`MARKS`].
fn copy_stick(stick: &Path, work: &Path) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let staging = work.join("staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let mut big = fs::File::create(staging.join(BIG))?;
    big.set_len(BIG_MIB << 20)?;
    for at in MARKS {
        big.seek(SeekFrom::Start(at))?;
        big.write_all(&(0..4096).map(|i| mark_byte(at, i)).collect::<Vec<u8>>())?;
    }
    drop(big);
    let _ = fs::remove_file(stick);
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b")
        .arg("4096")
        .arg("-L")
        .arg(COPY_LABEL)
        .arg("-d")
        .arg(&staging)
        .arg(stick)
        .arg((STICK_MIB * 256).to_string()))
}

fn run_copy_steps(s: &mut Session) -> R<()> {
    s.expect(", cmdline \"\"")?;
    s.expect(&format!("): ext4 '{COPY_LABEL}'; mounted at /storage/{COPY_LABEL} (rw)"))?;
    println!("  ok: the stick is mounted writable");
    if !s.expect_within("service-mgr: 'boot-probe' exited code=", std::time::Duration::from_secs(300))? {
        return Err("`boot-probe` did not exit within five minutes of the stick's mount".into());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    let (from, to) = (format!("/storage/{COPY_LABEL}/{BIG}"), format!("/storage/{COPY_LABEL}/{BIG_COPY}"));
    let started = std::time::Instant::now();
    let at = s.transcript().len();
    s.send(&format!("copy {from} {to}"))?;
    // The first line naming the copy with something after it: the table's row, its bytes, or an
    // error. The typed line names it last.
    let rest = s.line_since(at, &to, |rest| !rest.is_empty(), std::time::Duration::from_secs(600))?;
    if rest != (BIG_MIB << 20).to_string() {
        return Err(format!("the copy failed: {to}{rest}").into());
    }
    s.expect("/home>")?;
    println!("  ok: copied {BIG_MIB} MiB in {:.1} s", started.elapsed().as_secs_f64());
    eject_stick(s, &[COPY_LABEL])?;
    println!("  ok: ejected, the stick left clean");
    Ok(())
}

/// The stick once the machine has stopped: clean, and [`BIG_COPY`] the source's size with every
/// mark where the source had it and zeroes between.
fn check_copy_stick(stick: &Path) -> R<()> {
    check_ext4_clean(stick, "the stick")?;
    let copy = debugfs_cat(stick, &format!("/{BIG_COPY}"))?.ok_or("the stick holds no copy")?;
    if copy.len() as u64 != BIG_MIB << 20 {
        return Err(format!("the copy is {} bytes, not {}", copy.len(), BIG_MIB << 20).into());
    }
    for at in MARKS {
        let page = &copy[at as usize..at as usize + 4096];
        if page.iter().enumerate().any(|(i, &b)| b != mark_byte(at, i)) {
            return Err(format!("the copy's page at {at:#x} is not the source's").into());
        }
    }
    let zero = |at: u64| copy[at as usize..at as usize + 4096].iter().all(|&b| b == 0);
    if ![8 << 20, 100 << 20, 300 << 20].into_iter().all(zero) {
        return Err("the copy holds bytes where the source held none".into());
    }
    println!("  ok: the copy is {BIG_MIB} MiB, every mark in its place, the holes zero");
    Ok(())
}
