use std::collections::BTreeMap;

use bytes::Bytes;

use super::*;

#[test]
fn test_decode_integer() {
    assert_eq!(decode(b"i42e").unwrap(), Value::Integer(42));
    assert_eq!(decode(b"i-42e").unwrap(), Value::Integer(-42));
    assert_eq!(decode(b"i0e").unwrap(), Value::Integer(0));
}

#[test]
fn test_decode_integer_invalid() {
    assert!(decode(b"i-0e").is_err());
    assert!(decode(b"i03e").is_err());
    assert!(decode(b"ie").is_err());
}

#[test]
fn test_decode_bytes() {
    assert_eq!(
        decode(b"4:spam").unwrap(),
        Value::Bytes(Bytes::from_static(b"spam"))
    );
    assert_eq!(
        decode(b"0:").unwrap(),
        Value::Bytes(Bytes::from_static(b""))
    );
}

#[test]
fn test_decode_list() {
    let result = decode(b"l4:spami42ee").unwrap();
    match result {
        Value::List(l) => {
            assert_eq!(l.len(), 2);
            assert_eq!(l[0], Value::Bytes(Bytes::from_static(b"spam")));
            assert_eq!(l[1], Value::Integer(42));
        }
        _ => panic!("expected list"),
    }
}

#[test]
fn test_decode_dict() {
    let result = decode(b"d3:cow3:moo4:spam4:eggse").unwrap();
    match result {
        Value::Dict(d) => {
            assert_eq!(d.len(), 2);
            assert_eq!(
                d.get(&Bytes::from_static(b"cow")),
                Some(&Value::Bytes(Bytes::from_static(b"moo")))
            );
        }
        _ => panic!("expected dict"),
    }
}

#[test]
fn test_encode_integer() {
    assert_eq!(encode(&Value::Integer(42)).unwrap(), b"i42e");
    assert_eq!(encode(&Value::Integer(-42)).unwrap(), b"i-42e");
    assert_eq!(encode(&Value::Integer(0)).unwrap(), b"i0e");
}

#[test]
fn test_encode_bytes() {
    assert_eq!(
        encode(&Value::Bytes(Bytes::from_static(b"spam"))).unwrap(),
        b"4:spam"
    );
}

#[test]
fn test_encode_list() {
    let list = Value::List(vec![
        Value::Bytes(Bytes::from_static(b"spam")),
        Value::Integer(42),
    ]);
    assert_eq!(encode(&list).unwrap(), b"l4:spami42ee");
}

#[test]
fn test_encode_dict() {
    let mut dict = BTreeMap::new();
    dict.insert(
        Bytes::from_static(b"cow"),
        Value::Bytes(Bytes::from_static(b"moo")),
    );
    let value = Value::Dict(dict);
    assert_eq!(encode(&value).unwrap(), b"d3:cow3:mooe");
}

#[test]
fn test_roundtrip() {
    // Keys must be sorted lexicographically for bencode roundtrip
    let original = b"d8:announce15:http://test.com4:infod4:name4:test12:piece lengthi16384eee";
    let decoded = decode(original).unwrap();
    let encoded = encode(&decoded).unwrap();
    assert_eq!(encoded, original);
}

#[test]
fn test_nested_structures() {
    let data = b"d4:listl4:spami42eee";
    let decoded = decode(data).unwrap();
    let encoded = encode(&decoded).unwrap();
    assert_eq!(encoded, data);
}

#[test]
fn test_trailing_data_error() {
    assert!(decode(b"i42eextra").is_err());
}

#[test]
fn test_value_accessors() {
    let value = Value::Integer(42);
    assert_eq!(value.as_integer(), Some(42));
    assert!(value.as_bytes().is_none());

    let value = Value::Bytes(Bytes::from_static(b"test"));
    assert_eq!(value.as_str(), Some("test"));
    assert!(value.as_integer().is_none());

    let value = Value::List(vec![]);
    assert!(value.as_list().is_some());
    assert!(value.as_dict().is_none());
}
