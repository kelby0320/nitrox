//! Ejecting a stick: what an eject takes, and what is refused once it is ejected.

use std::string::String;
use std::vec::Vec;

use libkern::device::DeviceKind;

use crate::mounts::{
    EjectRefusal, Refusal, RereadRefusal, at, automount, eject, ejected, explicit, mark_ejected, reread, stop_said,
};
use crate::probe::Found;
use libinittoml::manifest::Mode;

use crate::table::{self, By, Device, Mounted};
use crate::tests::{decode, on_usb, rec, with_a_stick};

/// **A session ejects a stick, by the name it is mounted under**, and nothing else. **The name is
/// the mount's, not the filesystem's label**: two sticks labelled `DATA` are mounted `DATA` and
/// `DATA-2`, and each name ejects its own.
#[test]
fn a_session_ejects_a_stick_by_its_mount_name_and_nothing_internal() {
    let fat = |label: &str| Found::Fat { label: String::from(label), clean: Some(true), refused: None };
    let mut ds = with_a_stick(fat("DATA"));
    ds.push(Device { record: on_usb(rec(30, DeviceKind::Disk, 7, 29, "another stick", 65_536)), found: fat("DATA") });
    ds.push(Device { record: on_usb(rec(40, DeviceKind::Disk, 8, 39, "a third", 65_536)), found: fat("") });
    let init = [Mounted { device: 7, at: String::from("/"), by: By::Init, mode: Mode::Rw }];
    let mut mounted: Vec<Mounted> = init.to_vec();
    for p in automount(&ds, &init, true, true) {
        mounted.push(Mounted { device: p.device, at: at(&p.label), by: By::Storage, mode: p.mode });
    }
    let names: Vec<&str> = mounted.iter().filter_map(|m| m.at.strip_prefix("/storage/")).collect();
    assert_eq!(names, ["nitrox-live", "DATA", "DATA-2", "blk-8"], "the names the table's `mounted` column holds");
    let names = |n: &[&str]| -> Result<Vec<String>, EjectRefusal> { Ok(n.iter().map(|s| String::from(*s)).collect()) };
    assert_eq!(eject(&ds, &mounted, "DATA").map(|e| e.names), names(&["DATA"]));
    assert_eq!(
        eject(&ds, &mounted, "DATA-2").map(|e| e.names),
        names(&["DATA-2"]),
        "the second stick's own name, not its label"
    );
    assert_eq!(eject(&ds, &mounted, "blk-8").map(|e| e.names), names(&["blk-8"]), "a stick with no label");
    assert_eq!(
        eject(&ds, &mounted, "nitrox-live").map(|e| e.names),
        Err(EjectRefusal::NotRemovable),
        "a RAM disk's partition"
    );
    assert_eq!(
        eject(&ds, &mounted, "/").map(|e| e.names),
        Err(EjectRefusal::NotMounted),
        "init's root is not under /storage"
    );
    assert_eq!(eject(&ds, &mounted, "NXFAT").map(|e| e.names), Err(EjectRefusal::NotMounted));
    let by_init = [Mounted { device: 21, at: at("DATA"), by: By::Init, mode: Mode::Rw }];
    assert_eq!(
        eject(&ds, &by_init, "DATA").map(|e| e.names),
        Err(EjectRefusal::NotMounted),
        "`init`'s, wherever it put it"
    );
    assert_eq!(EjectRefusal::NotRemovable.kerror(), libkern::KError::NoAccess);
    assert!(core::str::from_utf8(EjectRefusal::NotRemovable.why()).unwrap().contains("with admin disk --unmount"));
}

#[test]
fn an_eject_takes_every_mount_on_the_stick() {
    let fat = |label: &str| Found::Fat { label: String::from(label), clean: Some(true), refused: None };
    let mut ds = with_a_stick(fat("ONE"));
    let second = rec(22, DeviceKind::Partition, 7, 20, "partition 2 (unlabelled)", 4_096);
    ds.push(Device { record: second, found: fat("TWO") });
    ds.push(Device { record: on_usb(rec(30, DeviceKind::Disk, 8, 29, "another stick", 65_536)), found: fat("OTHER") });
    let init = [Mounted { device: 7, at: String::from("/"), by: By::Init, mode: Mode::Rw }];
    let mut mounted: Vec<Mounted> = init.to_vec();
    for p in automount(&ds, &init, true, true) {
        mounted.push(Mounted { device: p.device, at: at(&p.label), by: By::Storage, mode: p.mode });
    }
    let both = Ok(std::vec![String::from("ONE"), String::from("TWO")]);
    assert_eq!(eject(&ds, &mounted, "ONE").map(|e| e.names), both);
    assert_eq!(eject(&ds, &mounted, "TWO").map(|e| e.names), both, "either partition names the stick");
    assert_eq!(eject(&ds, &mounted, "OTHER").map(|e| e.names), Ok(std::vec![String::from("OTHER")]));
    let without_one: Vec<Mounted> = mounted.iter().filter(|m| m.at != at("ONE")).cloned().collect();
    assert_eq!(
        eject(&ds, &without_one, "TWO").map(|e| e.names),
        Ok(std::vec![String::from("TWO")]),
        "what is left of it"
    );
    assert_eq!(eject(&ds, &mounted, "TWO").map(|e| e.disk), Ok(20), "a partition's disk is its parent");
    assert_eq!(eject(&ds, &mounted, "OTHER").map(|e| e.disk), Ok(30), "a stick its filesystem fills is the disk");
}

/// **An ejected disk and its partitions are read no more**: the table says to remove it, and a
/// mount or a `Reread` of either is refused, saying to plug it in again.
#[test]
fn an_ejected_stick_is_listed_to_be_removed_and_refused_a_mount_or_a_reread() {
    let mut ds = with_a_stick(Found::Ejected);
    let disk = ds.len() - 2;
    ds[disk].found = Found::Ejected;
    let t = decode(&table::all(&ds, &[]));
    let note = t.schema.fields.iter().position(|f| f.name == "note").unwrap();
    for row in &t.rows[t.rows.len() - 2..] {
        assert_eq!(row[note], libstream::Value::Str(String::from("ejected; remove it")));
    }
    let explicit = |name| explicit(&ds, &[], &[], true, true, name, "");
    assert_eq!(explicit("blk-6").map(|p| p.device), Err(Refusal::Ejected), "its partition");
    assert_eq!(explicit("blk-5").map(|p| p.device), Err(Refusal::Ejected), "and the disk");
    assert_eq!(reread(&ds, &[], true, "blk-6"), Err(RereadRefusal::Ejected));
    assert_eq!(reread(&ds, &[], true, "blk-5"), Err(RereadRefusal::Ejected));
    assert!(core::str::from_utf8(Refusal::Ejected.why()).unwrap().contains("plug it in again"));
}

/// **A stick the system runs from is never ejected**, as the whole disk stops: one the machine
/// started from, or one `init` mounted a filesystem on, a mount of this service's beside it.
#[test]
fn a_stick_the_system_runs_from_is_never_ejected() {
    let ext4 = |l: &str| Found::Ext4 { label: String::from(l), clean: Some(true) };
    let mut ds = with_a_stick(ext4("SIB"));
    ds.push(Device { record: rec(22, DeviceKind::Partition, 7, 20, "root", 4_096), found: ext4("ROOT") });
    let ours = Mounted { device: 21, at: at("SIB"), by: By::Storage, mode: Mode::Rw };
    assert_eq!(eject(&ds, std::slice::from_ref(&ours), "SIB").map(|e| e.disk), Ok(20), "the control: a stick as any");
    let inits = Mounted { device: 22, at: String::from("/data"), by: By::Init, mode: Mode::Rw };
    assert_eq!(eject(&ds, &[inits, ours.clone()], "SIB"), Err(EjectRefusal::SystemDisk), "init's beside it");
    let disk = ds.iter_mut().find(|d| d.record.id == 20).expect("the stick");
    disk.record.flags |= libkern::device::BOOT;
    assert_eq!(eject(&ds, &[ours], "SIB"), Err(EjectRefusal::SystemDisk), "the machine started from it");
    assert_eq!(EjectRefusal::SystemDisk.kerror(), libkern::KError::NoAccess);
}

#[test]
fn an_eject_marks_the_disk_and_its_partitions_and_nothing_else() {
    let mut ds = with_a_stick(Found::Ext4 { label: String::from("SIB"), clean: Some(true) });
    let before = ds.clone();
    mark_ejected(&mut ds, 20);
    for (d, was) in ds.iter().zip(&before) {
        let on_it = d.record.id == 20 || d.record.parent == 20;
        assert_eq!(ejected(&ds, d.record.id), on_it, "blk-{}", d.record.served);
        if !on_it {
            assert_eq!(d.found, was.found, "untouched");
        }
    }
}

#[test]
fn a_stop_is_said_as_it_went() {
    use libkern::KError;
    assert_eq!(stop_said(Ok(())), "stopped");
    assert_eq!(stop_said(Err(KError::Unsupported)), "did not stop: it cannot be stopped");
    assert_eq!(stop_said(Err(KError::IoError)), "did not stop: it failed the command");
    assert_eq!(stop_said(Err(KError::PeerClosed)), "did not stop: it went away");
    assert_eq!(stop_said(Err(KError::NotFound)), "did not stop: the kernel refused it");
}
