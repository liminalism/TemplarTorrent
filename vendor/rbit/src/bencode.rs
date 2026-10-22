//! Bencode encoding and decoding ([BEP-3]).
//!
//! Bencode is the serialization format used throughout BitTorrent for storing
//! and transmitting structured data, including `.torrent` files and tracker
//! responses.
//!
//! # Data Types
//!
//! Bencode supports four data types:
//!
//! | Type | Format | Example |
//! |------|--------|---------|
//! | Integer | `i<number>e` | `i42e` → 42 |
//! | Byte String | `<length>:<data>` | `4:spam` → "spam" |
//! | List | `l<items>e` | `l4:spami42ee` → ["spam", 42] |
//! | Dictionary | `d<key><value>...e` | `d3:foo3:bare` → {"foo": "bar"} |
//!
//! # Examples
//!
//! ## Decoding bencode data
//!
//! ```
//! use rbit::bencode::{decode, Value};
//!
//! // Decode an integer
//! let value = decode(b"i42e").unwrap();
//! assert_eq!(value.as_integer(), Some(42));
//!
//! // Decode a string
//! let value = decode(b"4:spam").unwrap();
//! assert_eq!(value.as_str(), Some("spam"));
//!
//! // Decode a list
//! let value = decode(b"l4:spami42ee").unwrap();
//! let list = value.as_list().unwrap();
//! assert_eq!(list.len(), 2);
//!
//! // Decode a dictionary
//! let value = decode(b"d3:foo3:bare").unwrap();
//! let foo = value.get(b"foo").unwrap();
//! assert_eq!(foo.as_str(), Some("bar"));
//! ```
//!
//! ## Encoding bencode data
//!
//! ```
//! use rbit::bencode::{encode, Value};
//! use bytes::Bytes;
//! use std::collections::BTreeMap;
//!
//! // Encode an integer
//! let encoded = encode(&Value::Integer(42)).unwrap();
//! assert_eq!(encoded, b"i42e");
//!
//! // Encode a string
//! let encoded = encode(&Value::string("hello")).unwrap();
//! assert_eq!(encoded, b"5:hello");
//!
//! // Encode a list
//! let list = Value::List(vec![
//!     Value::Integer(1),
//!     Value::Integer(2),
//! ]);
//! let encoded = encode(&list).unwrap();
//! assert_eq!(encoded, b"li1ei2ee");
//!
//! // Encode a dictionary
//! let mut dict = BTreeMap::new();
//! dict.insert(Bytes::from_static(b"key"), Value::string("value"));
//! let encoded = encode(&Value::Dict(dict)).unwrap();
//! assert_eq!(encoded, b"d3:key5:valuee");
//! ```
//!
//! ## Building complex structures
//!
//! ```
//! use rbit::bencode::Value;
//!
//! // Using From implementations for convenience
//! let int: Value = 42i64.into();
//! let string: Value = "hello".into();
//!
//! // Building a torrent-like structure
//! use std::collections::BTreeMap;
//! use bytes::Bytes;
//!
//! let mut info = BTreeMap::new();
//! info.insert(Bytes::from_static(b"name"), Value::string("example.txt"));
//! info.insert(Bytes::from_static(b"length"), Value::Integer(1024));
//! info.insert(Bytes::from_static(b"piece length"), Value::Integer(16384));
//!
//! let mut torrent = BTreeMap::new();
//! torrent.insert(Bytes::from_static(b"info"), Value::Dict(info));
//! torrent.insert(
//!     Bytes::from_static(b"announce"),
//!     Value::string("http://tracker.example.com/announce")
//! );
//! ```
//!
//! # Error Handling
//!
//! Decoding can fail for various reasons:
//!
//! - [`BencodeError::UnexpectedEof`] - Input ended unexpectedly
//! - [`BencodeError::InvalidInteger`] - Malformed integer (e.g., leading zeros)
//! - [`BencodeError::UnexpectedChar`] - Unexpected character in input
//! - [`BencodeError::NestingTooDeep`] - Recursion limit exceeded (max 64 levels)
//! - [`BencodeError::TrailingData`] - Extra data after the value
//!
//! [BEP-3]: http://bittorrent.org/beps/bep_0003.html

mod decode;
mod encode;
mod error;
mod value;

pub use decode::decode;
pub use encode::encode;
pub use error::BencodeError;
pub use value::Value;

#[cfg(test)]
mod tests;
