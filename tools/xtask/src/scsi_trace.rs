//! QEMU's SCSI trace: a gate's window onto what reaches a USB stick, as the PS/2 one is onto a key.

use std::path::Path;
use std::process::Command;

/// `START STOP UNIT` with LoEj set and Start clear.
const EJECT_CDB: [u8; 6] = [0x1B, 0, 0, 0, 0x02, 0];

/// Log every SCSI command QEMU's disks take, and every check condition they answer, to `log`.
pub(crate) fn trace_args(cmd: &mut Command, log: &Path) {
    cmd.arg("-trace").arg("scsi_disk_new_request").arg("-trace").arg("scsi_disk_check_condition").arg("-D").arg(log);
}

/// Where in `text` the first eject is traced, as a byte offset.
pub(crate) fn eject_traced(text: &str) -> Option<usize> {
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.contains("scsi_disk_new_request") && cdb(line) == EJECT_CDB {
            return Some(at);
        }
        at += line.len();
    }
    None
}

/// A trace line's CDB: `data= 0x1b 0x00 …` on CI's QEMU 8.2, `data=1b 00 …` on later ones.
fn cdb(line: &str) -> Vec<u8> {
    let Some(data) = line.split_once("data=").map(|(_, d)| d) else { return Vec::new() };
    data.split_whitespace().map_while(|b| u8::from_str_radix(b.trim_start_matches("0x"), 16).ok()).collect()
}

/// Whether a command after byte `from` met a medium gone (sense 2/58), as one to an ejected
/// stick does.
pub(crate) fn not_ready_after(text: &str, from: usize) -> bool {
    text.get(from..).unwrap_or("").lines().any(|l| l.contains("scsi_disk_check_condition") && l.contains("sense=2/58/"))
}

/// Whether `query-block`'s reply says device `id`'s tray is open; `None` for no such device.
pub(crate) fn tray_open(reply: &str, id: &str) -> Option<bool> {
    let peripheral = format!("/machine/peripheral/{id}/");
    devices(reply).into_iter().find(|d| d.contains(&peripheral)).map(|d| d.contains("\"tray_open\": true"))
}

/// The top-level objects of `query-block`'s `return` array, each as its text.
fn devices(reply: &str) -> Vec<&str> {
    let Some(start) = reply.find("\"return\": [").map(|i| i + "\"return\": [".len()) else {
        return Vec::new();
    };
    let (mut out, mut depth, mut open) = (Vec::new(), 0usize, 0);
    for (i, c) in reply[start..].char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    open = start + i;
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(&reply[open..=start + i]);
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eject_is_found_by_its_cdb_as_either_qemu_prints_it() {
        // As CI's QEMU 8.2 and QEMU 11 each wrote them.
        for (read, eject) in [
            ("data= 0x28 0x00 0x00 0x00 0x00 0x08 0x00 0x00 0x08 0x00", "data= 0x1b 0x00 0x00 0x00 0x02 0x00"),
            ("data=28 00 00 00 00 08 00 00 08 00", "data=1b 00 00 00 02 00"),
        ] {
            let log = format!(
                "scsi_disk_new_request Command: lun=0 tag=0x4 {read}\nscsi_disk_new_request Command: lun=0 tag=0x44 {eject}\n"
            );
            let at = eject_traced(&log).expect("the eject");
            assert!(log[at..].contains("tag=0x44"), "the line it is on");
        }
        let tagged = "scsi_disk_new_request Command: lun=0 tag=0x1b data=28 00 00 00 00 01 00 00 01 00";
        assert_eq!(eject_traced(tagged), None, "a tag is not a command");
        assert_eq!(
            eject_traced("scsi_disk_new_request Command: lun=0 tag=0x5 data=1b 00 00 00 01 00"),
            None,
            "a start"
        );
    }

    #[test]
    fn a_command_after_the_eject_is_found_by_its_sense() {
        let log = "scsi_disk_new_request Command: lun=0 tag=0x44 data=1b 00 00 00 02 00\n";
        let at = eject_traced(log).expect("the eject");
        let after = format!("{log}scsi_disk_check_condition Command complete tag=0x10 sense=2/58/0\n");
        assert!(not_ready_after(&after, at));
        assert!(!not_ready_after(&after, after.len()), "nothing after the end");
        let other = format!("{log}scsi_disk_check_condition Command complete tag=0x6 sense=6/40/0\n");
        assert!(!not_ready_after(&other, at), "a unit attention is not a missing medium");
    }

    #[test]
    fn the_tray_is_read_off_the_device_named() {
        let reply = r#"{"return": [{"io-status": "ok", "device": "", "locked": false, "removable": false, "inserted": {"ro": false, "image": {"filename": "a"}}, "qdev": "/machine/peripheral/boot/scsi.0/legacy[0]", "type": "unknown"}, {"device": "", "locked": false, "removable": true, "qdev": "/machine/peripheral/media/scsi.0/legacy[0]", "tray_open": true, "type": "unknown"}]}"#;
        assert_eq!(tray_open(reply, "media"), Some(true));
        assert_eq!(tray_open(reply, "boot"), Some(false));
        assert_eq!(tray_open(reply, "gone"), None);
    }
}
