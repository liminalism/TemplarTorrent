use std::collections::BTreeMap;

use bytes::Bytes;

/// A bencode value.
///
/// Bencode has four data types: integers, byte strings, lists, and dictionaries.
/// This enum represents any bencode value and provides methods for type-safe access.
///
/// # Examples
///
/// ```
/// use rbit::bencode::Value;
/// use bytes::Bytes;
/// use std::collections::BTreeMap;
///
/// // Creating values directly
/// let int = Value::Integer(42);
/// let string = Value::string("hello");
/// let list = Value::List(vec![Value::Integer(1), Value::Integer(2)]);
///
/// // Using From implementations
/// let int: Value = 42i64.into();
/// let string: Value = "hello".into();
///
/// // Accessing values
/// assert_eq!(int.as_integer(), Some(42));
/// assert_eq!(string.as_str(), Some("hello"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A signed 64-bit integer.
    Integer(i64),
    /// A byte string (may or may not be valid UTF-8).
    Bytes(Bytes),
    /// An ordered list of values.
    List(Vec<Value>),
    /// A dictionary with byte string keys (sorted by key in bencode encoding).
    Dict(BTreeMap<Bytes, Value>),
}

impl Value {
    /// Creates a byte string value from a UTF-8 string.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::Value;
    ///
    /// let value = Value::string("hello");
    /// assert_eq!(value.as_str(), Some("hello"));
    /// ```
    pub fn string(s: &str) -> Self {
        Value::Bytes(Bytes::copy_from_slice(s.as_bytes()))
    }

    /// Returns the value as an integer, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::Value;
    ///
    /// let int = Value::Integer(42);
    /// assert_eq!(int.as_integer(), Some(42));
    ///
    /// let string = Value::string("hello");
    /// assert_eq!(string.as_integer(), None);
    /// ```
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the value as a byte string, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::Value;
    ///
    /// let value = Value::string("hello");
    /// assert_eq!(value.as_bytes().map(|b| b.as_ref()), Some(b"hello".as_slice()));
    /// ```
    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the value as a UTF-8 string, if it is a valid UTF-8 byte string.
    ///
    /// Returns `None` if the value is not a byte string or if the bytes are not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::Value;
    ///
    /// let value = Value::string("hello");
    /// assert_eq!(value.as_str(), Some("hello"));
    ///
    /// let int = Value::Integer(42);
    /// assert_eq!(int.as_str(), None);
    /// ```
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// Returns the value as a list, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::Value;
    ///
    /// let list = Value::List(vec![Value::Integer(1), Value::Integer(2)]);
    /// assert_eq!(list.as_list().map(|l| l.len()), Some(2));
    /// ```
    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    /// Returns the value as a dictionary reference, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::{decode, Value};
    ///
    /// let value = decode(b"d3:foo3:bare").unwrap();
    /// let dict = value.as_dict().unwrap();
    /// assert!(dict.contains_key(b"foo".as_slice()));
    /// ```
    pub fn as_dict(&self) -> Option<&BTreeMap<Bytes, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Consumes the value and returns the dictionary, if it is one.
    ///
    /// This avoids cloning the dictionary when you need ownership.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::{decode, Value};
    ///
    /// let value = decode(b"d3:foo3:bare").unwrap();
    /// let dict = value.into_dict().unwrap();
    /// assert!(dict.contains_key(b"foo".as_slice()));
    /// ```
    pub fn into_dict(self) -> Option<BTreeMap<Bytes, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Looks up a key in this value if it is a dictionary.
    ///
    /// Returns `None` if the value is not a dictionary or if the key is not present.
    ///
    /// # Examples
    ///
    /// ```
    /// use rbit::bencode::decode;
    ///
    /// let value = decode(b"d3:foo3:bare").unwrap();
    /// assert_eq!(value.get(b"foo").and_then(|v| v.as_str()), Some("bar"));
    /// assert_eq!(value.get(b"missing"), None);
    /// ```
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict()?.get(key)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Integer(i)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::string(s)
    }
}

impl From<Bytes> for Value {
    fn from(b: Bytes) -> Self {
        Value::Bytes(b)
    }
}

impl From<Vec<Value>> for Value {
    fn from(l: Vec<Value>) -> Self {
        Value::List(l)
    }
}

impl From<BTreeMap<Bytes, Value>> for Value {
    fn from(d: BTreeMap<Bytes, Value>) -> Self {
        Value::Dict(d)
    }
}
