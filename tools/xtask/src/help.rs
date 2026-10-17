//! `cargo xtask help`: every command, a line or two each.

pub(crate) fn print_help() {
    println!(
        "Nitrox build orchestrator.\n\
         \n\
         Usage: cargo xtask <command> [args]\n\
         \n\
         Commands:\n  \
           build         build the kernel ELF\n  \
           image         build + assemble a UEFI-bootable disk image\n  \
           qemu          build + launch QEMU with OVMF\n  \
           qemu-debug    build + launch QEMU paused for GDB on :1234\n  \
           test          host-side unit tests (kernel lib + tools)\n  \
           test-qemu     boot a headless self-test image; pass/fail via isa-debug-exit\n  \
           test-interactive  boot the release image and drive a real login + shell\n  \
           check-terminal    click into nxterm, type, and check the shell's answer renders\n  \
           check-login       type a wrong then a right password at the graphical greeter\n  \
           check-logout      log out from the power menu: an editor's question, Cancel, the greeter\n  \
           check-fbcon       boot with no serial port; read the boot and a panic off the screen\n  \
           check-live        boot the live image as a USB stick: no disk, a RAM-disk root, a write\n  \
           check-report      pick the live menu's hardware report with no serial port; read its pages\n  \
           check-install     install to a blank disk from the live menu, then boot that disk\n  \
           check-recovery    reset a password on an installed disk from the live image, then boot it\n  \
           check-storage     the test live stick beside a copy of the release disk; the host checks it\n  \
           check-media       the live desktop: a stick plugged in, shown in Files, saved to, ejected\n  \
           check-shutdown    `with power shutdown` on a test disk; the host checks it; then a reboot\n  \
           check-resolutions run four display gates at five screen sizes; on demand, not in CI\n  \
           check-input       inject a key and a click; check both reach a userspace client\n  \
           \x20                `--no-ps2-irq` boots with the i8042's IRQs off, so the\n  \
           \x20                tick-driven recovery sweep is the only path input takes\n  \
           \x20                `--usb` (here, `check-login`, `check-report`): no i8042, and a\n  \
           \x20                USB keyboard and mouse, so every key and click is USB's\n  \
           check-display     boot + screendump; compare the screen to a libdraw render\n  \
           bench-compose     what composing a drag costs, and where (M13 Part A)\n  \
           throughput-stick  write the stick Part H's copies are timed with; `--check IMAGE`\n  \
           \x20                reads one back and checks every file by its pattern\n  \
           preview           render the toolkit here and write a PNG; `preview ui|ui-dark|term|all`\n  \
           tune              the compositing preview: shadows and the overview's opacity\n  \
           shot              boot the release image and photograph the desktop; `shot all`, or\n  \
           \x20                one of greeter|desktop|apps|windows|terminal|hover|files|editor|overview\n  \
           check-arch    fail if kernel code outside arch/ uses arch internals\n  \
           check-nightly fail if any crate uses a nightly `#![feature(...)]`\n  \
           check-deferrals fail if a `TODO(<tag>)` has no deferred-decisions.md entry\n  \
           check-docs      fail if a doc links to, or cites, a path that does not exist\n  \
           check-comments  fail if new comments exceed 0.15 of new code plus 3, or cite a PR or part;\n  \
           \x20               `--base REF` measures from its merge base (default origin/main);\n  \
           \x20               `--ratio DIR`: DIR's comments against 0.15 of its code, or its public\n  \
           \x20               items' docs; `--only-comments [REV]`: fail if REV changes code\n  \
           check-size      fail if a Rust file passes 1,000 lines or its baseline entry; `--update`\n  \
           \x20               lowers stale entries, `--rename OLD NEW` moves one with its file,\n  \
           \x20               `--split OLD NEW...` moves one onto what the file was split into\n  \
           check-moves     fail if a commit (default HEAD) changes anything but code it moves\n  \
           check-images    fail if a test image and a release image differ by anything new\n  \
           check-irq-scope fail if an interrupt entry stub skips the lock-ordering scope\n  \
           abi-sync-check  fail if userspace/libkern has drifted from the kernel ABI\n  \
           fetch-limine  download the pinned Limine binary tarball\n  \
           clean         remove build outputs and caches\n  \
           help          show this message\n\
         \n\
         `--selftest` (build/image/qemu) compiles + runs the boot self-tests / demos;\n         \
         without it the build boots straight to userspace.\n         \
         `--grab` (qemu/qemu-debug) hands the window the pointer and keyboard. The\n         \
         guest has a relative mouse and no absolute one, so ungrabbed its cursor and\n         \
         yours are two independent cursors that never line up, and every `Super`\n         \
         chord belongs to your desktop instead. Ctrl-Alt-G releases the grab.\n         \
         `--kvm` (any command that boots a guest) runs under hardware virtualisation\n         \
         instead of TCG — faster, and required on a host whose QEMU predates 9.0 (TCG\n         \
         emulates x2APIC only from 9.0, and this kernel is x2APIC-only).\n         \
         `--size WxH` (the screen gates, `shot`, `bench-compose`, `qemu`/`qemu-debug`)\n         \
         sets the screen QEMU boots. The gates default to 1360x768 and `qemu` to\n         \
         QEMU's 1280x800. The width must be a multiple of 8, or QEMU shears every row;\n         \
         `check-resolutions` runs the display gates at five sizes.\n         \
         Other args after `qemu` / `qemu-debug` are forwarded to QEMU.\n"
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_command_is_in_the_help() {
        let main = include_str!("main.rs");
        let dispatch = &main[main.find("let result = match cmd.as_deref()").unwrap()..];
        let dispatch = &dispatch[..dispatch.find("Some(other) =>").unwrap()];
        let help = include_str!("help.rs");
        let help = &help[help.find("fn print_help()").unwrap()..help.find("#[cfg(test)]").unwrap()];
        let commands: Vec<&str> = dispatch
            .split("Some(\"")
            .skip(1)
            .filter_map(|s| s.split('"').next())
            .filter(|c| !c.starts_with('-'))
            .collect();
        assert!(commands.len() > 30, "{commands:?}");
        for cmd in commands {
            assert!(help.contains(&format!(" {cmd} ")), "`cargo xtask {cmd}` is missing from print_help");
        }
    }
}
