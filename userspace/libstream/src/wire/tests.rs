use super::*;
use alloc::vec;

#[test]
fn type_tag_round_trips_and_rejects_unknown() {
    for tag in [
        TypeTag::Null,
        TypeTag::Bool,
        TypeTag::Int,
        TypeTag::Float,
        TypeTag::String,
        TypeTag::Bytes,
        TypeTag::Handle,
        TypeTag::List,
        TypeTag::Record,
        TypeTag::Error,
    ] {
        assert_eq!(TypeTag::from_u8(tag.to_u8()), Some(tag));
    }
    assert_eq!(TypeTag::from_u8(0x0A), None);
    assert_eq!(TypeTag::from_u8(0xFF), None);
}

#[test]
fn flags_and_modifiers() {
    let f = StreamFlags::TEXT_FALLBACK.union(StreamFlags::COMPRESSED);
    assert!(f.contains(StreamFlags::TEXT_FALLBACK));
    assert!(f.contains(StreamFlags::COMPRESSED));
    assert!(!StreamFlags::NONE.contains(StreamFlags::TEXT_FALLBACK));
    assert!(TypeModifiers::NULLABLE.contains(TypeModifiers::NULLABLE));
    assert!(!TypeModifiers::NONE.contains(TypeModifiers::NULLABLE));
}

fn value_round_trip(v: Value) {
    let tag = v.type_tag().expect("cell value has a tag");
    let mut buf = Vec::new();
    write_value(&mut buf, &v).unwrap();
    let mut src = ByteSource::new(&buf);
    let got = read_value(&mut src, tag).unwrap();
    assert_eq!(got, v);
    assert!(src.is_empty(), "value left trailing bytes");
}

#[test]
fn values_round_trip() {
    value_round_trip(Value::Bool(true));
    value_round_trip(Value::Bool(false));
    value_round_trip(Value::Int(-9_000_000_000));
    value_round_trip(Value::Int(i64::MIN));
    value_round_trip(Value::Float(3.5));
    value_round_trip(Value::Float(-0.0));
    value_round_trip(Value::Str(String::from("héllo, TSM1")));
    value_round_trip(Value::Str(String::new()));
    value_round_trip(Value::Bytes(vec![0, 1, 2, 254, 255]));
    value_round_trip(Value::Handle(0xDEAD_BEEF_0000_0042));
}

#[test]
fn list_values_round_trip() {
    value_round_trip(Value::List(Arc::from(&[][..])));
    value_round_trip(Value::List(Arc::from(&[Value::Int(1), Value::Int(2), Value::Int(3)][..])));
    value_round_trip(Value::List(Arc::from(
        &[Value::Int(7), Value::Str(String::from("mix")), Value::Bool(true), Value::Null][..],
    )));
    value_round_trip(Value::List(Arc::from(
        &[Value::List(Arc::from(&[Value::Int(1)][..])), Value::List(Arc::from(&[][..]))][..],
    )));
}

#[test]
fn record_values_round_trip() {
    let schema = Schema::new()
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("size", TypeTag::Int, TypeModifiers::NONE)
        .field("note", TypeTag::String, TypeModifiers::NULLABLE);
    value_round_trip(Value::Record(Arc::new(Record {
        schema: schema.clone(),
        values: vec![Value::Str(String::from("a")), Value::Int(10), Value::Str(String::from("ok"))],
    })));
    value_round_trip(Value::Record(Arc::new(Record {
        schema,
        values: vec![Value::Str(String::from("b")), Value::Int(20), Value::Null],
    })));
    let nested = Schema::new().field("tags", TypeTag::List, TypeModifiers::NONE);
    value_round_trip(Value::Record(Arc::new(Record {
        schema: nested,
        values: vec![Value::List(Arc::from(&[Value::Str(String::from("x")), Value::Str(String::from("y"))][..]))],
    })));
}

#[test]
fn table_value_round_trips_as_a_stream() {
    let schema = Schema::new()
        .field("pid", TypeTag::Int, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("parent", TypeTag::Handle, TypeModifiers::NULLABLE);
    let table = Table {
        flags: StreamFlags::NONE,
        schema,
        rows: vec![
            vec![Value::Int(1), Value::Str(String::from("init")), Value::Null],
            vec![Value::Int(2), Value::Str(String::from("fs")), Value::Handle(0x40)],
        ],
    };
    let mut buf = Vec::new();
    table.encode(&mut buf).unwrap();
    assert_eq!(&buf[..4], b"TSM1"); // a table serialises as a whole stream
    assert_eq!(Table::decode(&buf).unwrap(), table);
}

#[test]
fn table_is_not_a_cell_value() {
    let table = Value::Table(Arc::new(Table::default()));
    assert_eq!(table.type_tag(), None);
    let mut buf = Vec::new();
    assert_eq!(write_value(&mut buf, &table), Err(WireError::NestedTable));
    let list = Value::List(Arc::from(&[table][..]));
    assert_eq!(write_value(&mut Vec::new(), &list), Err(WireError::NestedTable));
}

#[test]
fn error_value_tag_is_unsupported() {
    let buf = [0u8; 8];
    let mut src = ByteSource::new(&buf);
    assert_eq!(read_value(&mut src, TypeTag::Error), Err(WireError::Unsupported(TypeTag::Error)));
}

#[test]
fn header_and_schema_round_trip() {
    let schema = Schema::new()
        .field("pid", TypeTag::Int, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE)
        .field("parent", TypeTag::Handle, TypeModifiers::NULLABLE);
    let flags = StreamFlags::TEXT_FALLBACK;

    let mut buf = Vec::new();
    encode_header(&mut buf, flags, &schema).unwrap();

    assert_eq!(&buf[..4], b"TSM1");

    let mut src = ByteSource::new(&buf);
    let (got_flags, got_schema) = decode_header(&mut src).unwrap();
    assert_eq!(got_flags, flags);
    assert_eq!(got_schema, schema);
    assert!(src.is_empty());
}

#[test]
fn empty_schema_is_valid() {
    let mut buf = Vec::new();
    encode_header(&mut buf, StreamFlags::NONE, &Schema::new()).unwrap();
    let mut src = ByteSource::new(&buf);
    let (_, schema) = decode_header(&mut src).unwrap();
    assert!(schema.fields.is_empty());
}

#[test]
fn bad_magic_is_rejected() {
    let buf = *b"XSM1\x00\x00\x00\x00";
    let mut src = ByteSource::new(&buf);
    assert_eq!(decode_header(&mut src), Err(WireError::BadMagic));
}

#[test]
fn truncated_input_is_eof_not_panic() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAGIC);
    put_u32(&mut buf, 0).unwrap(); // flags
    put_u32(&mut buf, 1).unwrap(); // field_count = 1
    put_u16(&mut buf, 5).unwrap(); // name_len = 5, but no name bytes follow
    let mut src = ByteSource::new(&buf);
    assert_eq!(decode_header(&mut src), Err(WireError::UnexpectedEof));
}

#[test]
fn unknown_type_tag_in_schema_is_rejected() {
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAGIC);
    put_u32(&mut buf, 0).unwrap(); // flags
    put_u32(&mut buf, 1).unwrap(); // one field
    put_u16(&mut buf, 1).unwrap();
    buf.push(b'x'); // name "x"
    buf.push(0x7E); // bogus type tag
    buf.push(0); // modifiers
    let mut src = ByteSource::new(&buf);
    assert_eq!(decode_header(&mut src), Err(WireError::BadTypeTag(0x7E)));
}

#[test]
fn terminator_round_trips() {
    let mut buf = Vec::new();
    encode_terminator(&mut buf, -7).unwrap();
    let mut src = ByteSource::new(&buf);
    assert_eq!(src.u8().unwrap(), REC_TERMINATOR);
    assert_eq!(src.i32().unwrap(), -7);
    assert!(src.is_empty());
}

#[test]
fn error_record_round_trips() {
    let rec = WireErrorRecord { code: -13, message: String::from("type mismatch"), field_name: String::from("cpu") };
    let mut buf = Vec::new();
    rec.encode(&mut buf).unwrap();
    let mut src = ByteSource::new(&buf);
    assert_eq!(src.u8().unwrap(), REC_ERROR); // record loop reads the tag...
    assert_eq!(WireErrorRecord::decode_body(&mut src).unwrap(), rec); // ...then the body
    assert!(src.is_empty());
}
