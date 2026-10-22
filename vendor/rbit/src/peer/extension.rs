use std::collections::BTreeMap;

use bytes::Bytes;

use super::error::PeerError;
use crate::bencode::{decode, encode, Value};

pub const EXTENSION_HANDSHAKE_ID: u8 = 0;

#[derive(Debug, Clone, Default)]
pub struct ExtensionHandshake {
    pub extensions: BTreeMap<String, u8>,
    pub client: Option<String>,
    pub yourip: Option<Vec<u8>>,
    pub reqq: Option<i64>,
    pub metadata_size: Option<i64>,
}

impl ExtensionHandshake {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_extensions(extensions: &[(&str, u8)]) -> Self {
        let mut hs = Self::new();
        for (name, id) in extensions {
            hs.extensions.insert((*name).to_string(), *id);
        }
        hs
    }

    pub fn encode(&self) -> Result<Bytes, PeerError> {
        let mut dict = BTreeMap::new();

        let mut m = BTreeMap::new();
        for (name, id) in &self.extensions {
            m.insert(
                Bytes::copy_from_slice(name.as_bytes()),
                Value::Integer(*id as i64),
            );
        }
        dict.insert(Bytes::from_static(b"m"), Value::Dict(m));

        if let Some(ref client) = self.client {
            dict.insert(Bytes::from_static(b"v"), Value::string(client));
        }

        if let Some(ref ip) = self.yourip {
            dict.insert(
                Bytes::from_static(b"yourip"),
                Value::Bytes(Bytes::copy_from_slice(ip)),
            );
        }

        if let Some(reqq) = self.reqq {
            dict.insert(Bytes::from_static(b"reqq"), Value::Integer(reqq));
        }

        if let Some(size) = self.metadata_size {
            dict.insert(Bytes::from_static(b"metadata_size"), Value::Integer(size));
        }

        let encoded = encode(&Value::Dict(dict))?;
        Ok(Bytes::from(encoded))
    }

    pub fn decode(data: &[u8]) -> Result<Self, PeerError> {
        let value = decode(data)?;
        let dict = value
            .as_dict()
            .ok_or_else(|| PeerError::Extension("expected dict".into()))?;

        let mut hs = Self::new();

        if let Some(m) = dict.get(b"m".as_slice()).and_then(|v| v.as_dict()) {
            for (key, val) in m {
                if let (Ok(name), Some(id)) = (std::str::from_utf8(key), val.as_integer()) {
                    if id > 0 {
                        hs.extensions.insert(name.to_string(), id as u8);
                    }
                }
            }
        }

        hs.client = dict
            .get(b"v".as_slice())
            .and_then(|v| v.as_str())
            .map(String::from);

        hs.yourip = dict
            .get(b"yourip".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec());

        hs.reqq = dict.get(b"reqq".as_slice()).and_then(|v| v.as_integer());

        hs.metadata_size = dict
            .get(b"metadata_size".as_slice())
            .and_then(|v| v.as_integer());

        Ok(hs)
    }

    pub fn get_extension_id(&self, name: &str) -> Option<u8> {
        self.extensions.get(name).copied()
    }
}

#[derive(Debug, Clone)]
pub enum ExtensionMessage {
    Handshake(ExtensionHandshake),
    Unknown { id: u8, payload: Bytes },
}

impl ExtensionMessage {
    pub fn decode(id: u8, payload: &[u8]) -> Result<Self, PeerError> {
        if id == EXTENSION_HANDSHAKE_ID {
            let hs = ExtensionHandshake::decode(payload)?;
            Ok(ExtensionMessage::Handshake(hs))
        } else {
            Ok(ExtensionMessage::Unknown {
                id,
                payload: Bytes::copy_from_slice(payload),
            })
        }
    }
}
