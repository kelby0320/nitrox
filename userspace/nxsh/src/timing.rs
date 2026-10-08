//! **`time PIPELINE`'s line** (Phase 6 Part H.2, §3): how long a pipeline took, and what each disk
//! and the page cache did while it ran — the device manager's `io.tsm`, read before and after, and
//! the difference. Pure: the evaluator reads the clock and the table, and this turns two readings
//! into the line.
//!
//! **One line, ASCII only**, since it is read off a laptop's screen during a measurement and off a
//! serial console by a gate, which drops everything else:
//!
//! ```text
//! time: 12.34 s; blk-0: 1,200 reads 4.7 MiB, 300 writes 1.2 MiB, busy 3.21 s; page-cache: 1,200 fills 4.7 MiB, 300 write-backs 1.2 MiB
//! ```
//!
//! A device whose counts did not move is left out, and so is a count that did not; a device's busy
//! time is always said. With no table to read, the line is the time alone.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libstream::wire::{Table, Value};

/// Where the counts are: the device manager's table, bound in every session at `/dev/devices`.
pub const IO_TABLE: &str = "/dev/devices/io.tsm";

/// The row the page cache's counts are in.
const PAGE_CACHE: &str = "page-cache";

/// One row of `io.tsm`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IoRow {
    pub name: String,
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub errors: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub busy_ns: u64,
    pub hits: u64,
}

/// The rows of an `io.tsm`'s bytes, or `None` for bytes that are not one. A column it lacks reads
/// as zero, and so does a `Null` cell.
pub fn rows(bytes: &[u8]) -> Option<Vec<IoRow>> {
    let t = Table::decode(bytes).ok()?;
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let name_at = col("name")?;
    let num = |row: &[Value], name: &str| match col(name).and_then(|i| row.get(i)) {
        Some(Value::Int(v)) if *v > 0 => *v as u64,
        _ => 0,
    };
    let mut out = Vec::new();
    for row in &t.rows {
        let Some(Value::Str(name)) = row.get(name_at) else { continue };
        out.push(IoRow {
            name: name.clone(),
            reads: num(row, "reads"),
            writes: num(row, "writes"),
            flushes: num(row, "flushes"),
            errors: num(row, "errors"),
            read_bytes: num(row, "read_bytes"),
            written_bytes: num(row, "written_bytes"),
            busy_ns: num(row, "busy_ns"),
            hits: num(row, "hits"),
        });
    }
    Some(out)
}

/// **The line**: `wall_ns` if the host has a clock, then what moved between `before` and `after`. A
/// row in `after` and not in `before` — a stick plugged in meanwhile — counts from nothing.
pub fn line(wall_ns: Option<u64>, before: Option<&[IoRow]>, after: Option<&[IoRow]>) -> String {
    let mut out = String::from("time: ");
    out.push_str(&wall_ns.map_or_else(|| String::from("unknown"), duration));
    let (Some(before), Some(after)) = (before, after) else {
        return out;
    };
    let mut moved = false;
    for a in after {
        let zero = IoRow::default();
        let b = before.iter().find(|b| b.name == a.name).unwrap_or(&zero);
        let d = |f: fn(&IoRow) -> u64| f(a).saturating_sub(f(b));
        let mut parts: Vec<String> = Vec::new();
        if a.name == PAGE_CACHE {
            if d(|r| r.reads) > 0 {
                parts.push(format!("{} fills {}", group(d(|r| r.reads)), bytes(d(|r| r.read_bytes))));
            }
            if d(|r| r.writes) > 0 {
                parts.push(format!("{} write-backs {}", group(d(|r| r.writes)), bytes(d(|r| r.written_bytes))));
            }
            if d(|r| r.hits) > 0 && !parts.is_empty() {
                parts.push(format!("{} hits", group(d(|r| r.hits))));
            }
        } else {
            if d(|r| r.reads) + d(|r| r.writes) + d(|r| r.flushes) + d(|r| r.errors) == 0 {
                continue;
            }
            if d(|r| r.reads) > 0 {
                parts.push(format!("{} reads {}", group(d(|r| r.reads)), bytes(d(|r| r.read_bytes))));
            }
            if d(|r| r.writes) > 0 {
                parts.push(format!("{} writes {}", group(d(|r| r.writes)), bytes(d(|r| r.written_bytes))));
            }
            if d(|r| r.flushes) > 0 {
                parts.push(format!("{} flushes", group(d(|r| r.flushes))));
            }
            if d(|r| r.errors) > 0 {
                parts.push(format!("{} errors", group(d(|r| r.errors))));
            }
            parts.push(format!("busy {}", duration(d(|r| r.busy_ns))));
        }
        if parts.is_empty() {
            continue;
        }
        moved = true;
        out.push_str("; ");
        out.push_str(&a.name);
        out.push_str(": ");
        out.push_str(&parts.join(", "));
    }
    if !moved {
        out.push_str("; no disk I/O");
    }
    out
}

/// A duration, rounded: seconds to the hundredth from one second, else milliseconds, else
/// microseconds.
fn duration(ns: u64) -> String {
    let round = |unit: u64| (ns + unit / 2) / unit;
    if ns >= 1_000_000_000 {
        let centis = round(10_000_000);
        format!("{}.{:02} s", group(centis / 100), centis % 100)
    } else if ns >= 1_000_000 {
        format!("{} ms", round(1_000_000))
    } else {
        format!("{} us", round(1_000))
    }
}

/// A size: bytes below a KiB, else KiB, MiB or GiB to the nearest tenth.
fn bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, size) in UNITS {
        if n >= size {
            let tenths = (n * 10 + size / 2) / size;
            return format!("{}.{} {unit}", group(tenths / 10), tenths % 10);
        }
    }
    format!("{n} B")
}

/// A count with its thousands separated by commas.
fn group(n: u64) -> String {
    let digits = format!("{n}");
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use libstream::wire::{Schema, StreamFlags, TypeModifiers, TypeTag};

    fn row(name: &str, reads: u64, read_bytes: u64, writes: u64, written_bytes: u64, busy_ns: u64) -> IoRow {
        IoRow { name: name.into(), reads, read_bytes, writes, written_bytes, busy_ns, ..Default::default() }
    }

    /// **The line names what moved and nothing else**: the time, a disk that read and wrote with
    /// its busy time, the page cache's fills and write-backs — and not the disk that did nothing, nor
    /// the counts of the one that did that stayed still. A stick that arrived meanwhile counts from
    /// nothing.
    #[test]
    fn the_line_names_what_moved() {
        let before = [row("blk-0", 100, 1 << 20, 10, 4096, 5_000_000), row("blk-2", 7, 512, 0, 0, 1), row("page-cache", 50, 50 * 4096, 2, 8192, 0)];
        let after = [
            row("blk-0", 1300, (1 << 20) + 5_033_164, 310, 4096 + (5 << 20), 3_215_000_000),
            row("blk-2", 7, 512, 0, 0, 1),
            row("page-cache", 1250, 1250 * 4096, 302, 8192 + 300 * 4096, 0),
            row("blk-5", 0, 0, 2, 1024, 40_000),
        ];
        assert_eq!(
            line(Some(12_345_678_901), Some(&before), Some(&after)),
            "time: 12.35 s; blk-0: 1,200 reads 4.8 MiB, 300 writes 5.0 MiB, busy 3.21 s; \
             page-cache: 1,200 fills 4.7 MiB, 300 write-backs 1.2 MiB; blk-5: 2 writes 1.0 KiB, busy 40 us"
        );
    }

    /// With no table, the time alone; with nothing moved, the time and that; with no clock, so.
    #[test]
    fn what_the_line_says_with_less_to_say() {
        let still = [row("blk-0", 1, 512, 0, 0, 9)];
        assert_eq!(line(Some(850_000_000), None, None), "time: 850 ms");
        assert_eq!(line(Some(1_400), Some(&still), Some(&still)), "time: 1 us; no disk I/O");
        assert_eq!(line(None, Some(&still), None), "time: unknown");
    }

    /// The table as the device manager encodes it — `Null` where a row has no count — reads into
    /// rows, and bytes that are not a table do not.
    #[test]
    fn rows_read_from_the_table_and_its_nulls_as_zero() {
        let int = |s: Schema, n: &str| s.field(n, TypeTag::Int, TypeModifiers::NULLABLE);
        let schema = ["reads", "writes", "flushes", "errors", "read_bytes", "written_bytes", "busy_ns", "hits", "holes"]
            .into_iter()
            .fold(Schema::new().field("name", TypeTag::String, TypeModifiers::NONE), int);
        let n = |v: i64| Value::Int(v);
        let t = Table {
            flags: StreamFlags::NONE,
            schema,
            rows: vec![
                vec![Value::Str("blk-0".into()), n(3), n(4), n(5), n(0), n(6), n(7), n(8), Value::Null, Value::Null],
                vec![Value::Str("page-cache".into()), n(9), n(1), Value::Null, Value::Null, n(2), n(3), n(4), n(10), n(11)],
            ],
        };
        let mut bytes = Vec::new();
        t.encode(&mut bytes).unwrap();
        let r = rows(&bytes).unwrap();
        assert_eq!(r[0], IoRow { name: "blk-0".into(), reads: 3, writes: 4, flushes: 5, read_bytes: 6, written_bytes: 7, busy_ns: 8, ..Default::default() });
        assert_eq!((r[1].flushes, r[1].hits), (0, 10));
        assert!(rows(b"not a table").is_none());
    }

    #[test]
    fn sizes_and_counts_read_as_a_person_reads_them() {
        assert_eq!((group(0), group(999), group(1000), group(1_234_567)), ("0".into(), "999".into(), "1,000".into(), "1,234,567".into()));
        assert_eq!((bytes(1023), bytes(1024), bytes(1536), bytes(3 << 30)), ("1023 B".into(), "1.0 KiB".into(), "1.5 KiB".into(), "3.0 GiB".into()));
        assert_eq!((duration(499), duration(1_499_999), duration(61_004_999_999)), ("0 us".into(), "1 ms".into(), "61.00 s".into()));
        assert_eq!((duration(500), duration(999_999_999), duration(1_005_000_000)), ("1 us".into(), "1000 ms".into(), "1.01 s".into()));
    }
}
