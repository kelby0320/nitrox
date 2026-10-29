use super::*;
use alloc::vec;

fn list_of(v: Value) -> Value {
    Value::List(Arc::from(vec![v]))
}

fn record_of(v: Value) -> Value {
    let ty = v.type_tag().unwrap();
    let schema = Schema::new().field("x", ty, TypeModifiers::NONE);
    Value::Record(Arc::new(Record { schema, values: vec![v] }))
}

/// `depth` nested one-element lists around an empty one.
fn nested_lists(depth: usize) -> Value {
    let mut v = Value::List(Arc::from(Vec::new()));
    for _ in 1..depth {
        v = list_of(v);
    }
    v
}

/// The bytes of `depth` levels alternating Record, List, Record, ... around an empty body.
fn mixed_bytes(depth: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for level in 0..depth {
        if level + 1 == depth {
            out.extend_from_slice(&[0; 4]);
        } else if level % 2 == 0 {
            let next = if (level + 1) % 2 == 0 { TypeTag::Record } else { TypeTag::List };
            out.extend_from_slice(&[1, 0, 0, 0, 1, 0, b'x', next.to_u8(), 0]);
        } else {
            let next = if (level + 1) % 2 == 0 { TypeTag::Record } else { TypeTag::List };
            out.extend_from_slice(&[1, 0, 0, 0, next.to_u8()]);
        }
    }
    out
}

fn nested_list_bytes(depth: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 1..depth {
        out.extend_from_slice(&[1, 0, 0, 0, TypeTag::List.to_u8()]);
    }
    out.extend_from_slice(&[0; 4]);
    out
}

fn decode(bytes: &[u8], tag: TypeTag) -> Result<Value> {
    read_value(&mut ByteSource::new(bytes), tag)
}

#[test]
fn max_depth_lists_round_trip() {
    let v = nested_lists(MAX_DEPTH);
    let mut buf = Vec::new();
    write_value(&mut buf, &v).unwrap();
    assert_eq!(buf, nested_list_bytes(MAX_DEPTH));
    assert_eq!(decode(&buf, TypeTag::List), Ok(v));
}

#[test]
fn one_level_past_max_depth_is_refused_both_ways() {
    let bytes = nested_list_bytes(MAX_DEPTH + 1);
    assert_eq!(decode(&bytes, TypeTag::List), Err(WireError::TooLarge));
    let v = nested_lists(MAX_DEPTH + 1);
    assert_eq!(write_value(&mut Vec::new(), &v), Err(WireError::TooLarge));
}

#[test]
fn records_and_lists_count_together() {
    let bytes = mixed_bytes(MAX_DEPTH);
    let v = decode(&bytes, TypeTag::Record).unwrap();
    let mut again = Vec::new();
    write_value(&mut again, &v).unwrap();
    assert_eq!(again, bytes);

    assert_eq!(decode(&mixed_bytes(MAX_DEPTH + 1), TypeTag::Record), Err(WireError::TooLarge));
    assert_eq!(write_value(&mut Vec::new(), &list_of(v)), Err(WireError::TooLarge));
}

#[test]
fn a_record_in_a_list_in_a_record_is_three_levels() {
    let wrapped = |lists: usize| record_of(list_of(record_of(nested_lists(lists))));
    assert_eq!(write_value(&mut Vec::new(), &wrapped(MAX_DEPTH - 3)), Ok(()));
    assert_eq!(write_value(&mut Vec::new(), &wrapped(MAX_DEPTH - 2)), Err(WireError::TooLarge));
}

#[test]
fn a_very_deep_list_is_refused_without_overflowing_the_stack() {
    let bytes = nested_list_bytes(40_000);
    assert_eq!(decode(&bytes, TypeTag::List), Err(WireError::TooLarge));
}

fn null_table(columns: usize, rows: usize) -> Vec<u8> {
    let mut schema = Schema::new();
    for _ in 0..columns {
        schema = schema.field("", TypeTag::Null, TypeModifiers::NONE);
    }
    let mut buf = Vec::new();
    encode_header(&mut buf, StreamFlags::NONE, &schema).unwrap();
    buf.extend(core::iter::repeat_n(REC_DATA, rows));
    encode_terminator(&mut buf, 0).unwrap();
    buf
}

#[test]
fn zero_byte_cells_cannot_outgrow_the_input() {
    let buf = null_table(1_000, 1_000);
    assert!(buf.len() < 6_000);
    assert_eq!(Table::decode(&buf), Err(WireError::TooLarge));
}

#[test]
fn the_value_budget_is_exact() {
    // Two Null columns: each row is one byte and two values.
    let rows = 4_121;
    let buf = null_table(2, rows);
    assert_eq!(rows * 2, buf.len() + VALUE_FLOOR);
    assert_eq!(Table::decode(&buf).unwrap().rows.len(), rows);

    let over = null_table(2, rows + 1);
    assert_eq!((rows + 1) * 2, over.len() + VALUE_FLOOR + 1);
    assert_eq!(Table::decode(&over), Err(WireError::TooLarge));
}

#[test]
fn an_ordinary_table_is_within_budget() {
    let schema =
        Schema::new().field("n", TypeTag::Int, TypeModifiers::NONE).field("s", TypeTag::String, TypeModifiers::NONE);
    let rows = (0..10_000).map(|i| vec![Value::Int(i), Value::Str(String::from("x"))]).collect();
    let table = Table { flags: StreamFlags::NONE, schema, rows };
    let mut buf = Vec::new();
    table.encode(&mut buf).unwrap();
    assert_eq!(Table::decode(&buf), Ok(table));
}

fn name(len: usize) -> String {
    "n".repeat(len)
}

#[test]
fn a_schema_name_over_u16_is_refused_before_any_write() {
    let schema = Schema::new().field("ok", TypeTag::Int, TypeModifiers::NONE).field(
        &name(usize::from(u16::MAX) + 1),
        TypeTag::Int,
        TypeModifiers::NONE,
    );
    let mut buf = Vec::new();
    assert_eq!(schema.encode(&mut buf), Err(WireError::TooLarge));
    assert!(buf.is_empty());
}

#[test]
fn a_schema_name_of_u16_max_round_trips() {
    let schema = Schema::new().field(&name(usize::from(u16::MAX)), TypeTag::Int, TypeModifiers::NONE);
    let mut buf = Vec::new();
    schema.encode(&mut buf).unwrap();
    assert_eq!(Schema::decode(&mut ByteSource::new(&buf)), Ok(schema));
}

#[test]
fn an_error_record_field_name_over_u16_is_refused_before_any_write() {
    let rec = WireErrorRecord { code: 1, message: String::from("m"), field_name: name(usize::from(u16::MAX) + 1) };
    let mut buf = Vec::new();
    assert_eq!(rec.encode(&mut buf), Err(WireError::TooLarge));
    assert!(buf.is_empty());
}

#[test]
fn an_error_record_field_name_of_u16_max_round_trips() {
    let rec = WireErrorRecord { code: 1, message: String::from("m"), field_name: name(usize::from(u16::MAX)) };
    let mut buf = Vec::new();
    rec.encode(&mut buf).unwrap();
    let mut src = ByteSource::new(&buf);
    assert_eq!(src.u8(), Ok(REC_ERROR));
    assert_eq!(WireErrorRecord::decode_body(&mut src), Ok(rec));
}

/// **An absent nullable value spends from the budget too**, as the spec says every value does: a
/// row of an absent nullable `Int` and two `Null`s is three values in two bytes.
#[test]
fn an_absent_nullable_value_spends_from_the_budget() {
    let schema = Schema::new()
        .field("a", TypeTag::Int, TypeModifiers::NULLABLE)
        .field("b", TypeTag::Null, TypeModifiers::NONE)
        .field("c", TypeTag::Null, TypeModifiers::NONE);
    let rows = |n: usize| {
        let mut buf = Vec::new();
        encode_header(&mut buf, StreamFlags::NONE, &schema).unwrap();
        for _ in 0..n {
            buf.extend_from_slice(&[REC_DATA, 0]);
        }
        encode_terminator(&mut buf, 0).unwrap();
        buf
    };
    let past = rows(10_000);
    assert!(3 * 10_000 > past.len() + VALUE_FLOOR);
    assert_eq!(Table::decode(&past).map(|t| t.rows.len()), Err(WireError::TooLarge));
    let within = rows(4_000);
    assert!(3 * 4_000 <= within.len() + VALUE_FLOOR);
    assert_eq!(Table::decode(&within).map(|t| t.rows.len()), Ok(4_000));
}

/// **Depth is a value's nesting, not a stream's count of nested values**: a list of 100 lists
/// decodes, each level given back when its value ends.
#[test]
fn sibling_lists_give_their_level_back() {
    let siblings = Value::List(Arc::from((0..100).map(|_| nested_lists(2)).collect::<Vec<_>>()));
    let mut buf = Vec::new();
    write_value(&mut buf, &siblings).unwrap();
    assert_eq!(decode(&buf, TypeTag::List), Ok(siblings));
}

/// **A chain of records alone is held to the depth**, as a chain of lists is.
#[test]
fn a_chain_of_records_is_held_to_the_depth() {
    let chain = |levels: usize| (1..levels).fold(record_of(Value::Int(1)), |v, _| record_of(v));
    let mut buf = Vec::new();
    write_value(&mut buf, &chain(MAX_DEPTH)).unwrap();
    assert_eq!(decode(&buf, TypeTag::Record), Ok(chain(MAX_DEPTH)));
    assert_eq!(write_value(&mut Vec::new(), &chain(MAX_DEPTH + 1)), Err(WireError::TooLarge));
}
