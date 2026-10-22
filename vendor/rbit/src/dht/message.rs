use std::collections::BTreeMap;
use std::net::SocketAddr;

use bytes::Bytes;

use super::error::DhtError;
use super::node::{Node, NodeId};
use crate::bencode::{decode, encode, Value};

pub type TransactionId = Bytes;

#[derive(Debug, Clone)]
pub enum DhtQuery {
    Ping,
    FindNode {
        target: NodeId,
    },
    GetPeers {
        info_hash: [u8; 20],
    },
    AnnouncePeer {
        info_hash: [u8; 20],
        port: u16,
        token: Bytes,
        implied_port: bool,
    },
}

#[derive(Debug, Clone)]
pub enum DhtResponse {
    Ping {
        id: NodeId,
    },
    FindNode {
        id: NodeId,
        nodes: Vec<Node>,
    },
    GetPeers {
        id: NodeId,
        token: Bytes,
        peers: Option<Vec<SocketAddr>>,
        nodes: Option<Vec<Node>>,
    },
    AnnouncePeer {
        id: NodeId,
    },
    Error {
        code: i64,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct DhtMessage {
    pub transaction_id: TransactionId,
    pub sender_id: Option<NodeId>,
    pub query: Option<(String, DhtQuery)>,
    pub response: Option<DhtResponse>,
    /// BEP-42: Compact IP + port of the requestor (included in responses)
    pub ip: Option<Bytes>,
}

impl DhtMessage {
    pub fn ping(transaction_id: TransactionId, our_id: &NodeId) -> Self {
        Self {
            transaction_id,
            sender_id: Some(*our_id),
            query: Some(("ping".to_string(), DhtQuery::Ping)),
            response: None,
            ip: None,
        }
    }

    pub fn find_node(transaction_id: TransactionId, our_id: &NodeId, target: NodeId) -> Self {
        Self {
            transaction_id,
            sender_id: Some(*our_id),
            query: Some(("find_node".to_string(), DhtQuery::FindNode { target })),
            response: None,
            ip: None,
        }
    }

    pub fn get_peers(transaction_id: TransactionId, our_id: &NodeId, info_hash: [u8; 20]) -> Self {
        Self {
            transaction_id,
            sender_id: Some(*our_id),
            query: Some(("get_peers".to_string(), DhtQuery::GetPeers { info_hash })),
            response: None,
            ip: None,
        }
    }

    pub fn announce_peer(
        transaction_id: TransactionId,
        our_id: &NodeId,
        info_hash: [u8; 20],
        port: u16,
        token: Bytes,
    ) -> Self {
        Self {
            transaction_id,
            sender_id: Some(*our_id),
            query: Some((
                "announce_peer".to_string(),
                DhtQuery::AnnouncePeer {
                    info_hash,
                    port,
                    token,
                    implied_port: false,
                },
            )),
            response: None,
            ip: None,
        }
    }

    pub fn parse(data: &[u8]) -> Result<Self, DhtError> {
        let value = decode(data)?;

        let dict = value
            .as_dict()
            .ok_or_else(|| DhtError::InvalidMessage("expected dict".into()))?;

        let transaction_id = dict
            .get(b"t".as_slice())
            .and_then(|v| v.as_bytes())
            .cloned()
            .ok_or_else(|| DhtError::InvalidMessage("missing transaction id".into()))?;

        let msg_type = dict
            .get(b"y".as_slice())
            .and_then(|v| v.as_str())
            .ok_or_else(|| DhtError::InvalidMessage("missing message type".into()))?;

        match msg_type {
            "q" => Self::parse_query(transaction_id, dict),
            "r" => Self::parse_response(transaction_id, dict),
            "e" => Self::parse_error(transaction_id, dict),
            _ => Err(DhtError::InvalidMessage(format!(
                "unknown message type: {}",
                msg_type
            ))),
        }
    }

    fn parse_query(
        transaction_id: TransactionId,
        dict: &BTreeMap<Bytes, Value>,
    ) -> Result<Self, DhtError> {
        let query_name = dict
            .get(b"q".as_slice())
            .and_then(|v| v.as_str())
            .ok_or_else(|| DhtError::InvalidMessage("missing query name".into()))?;

        let args = dict
            .get(b"a".as_slice())
            .and_then(|v| v.as_dict())
            .ok_or_else(|| DhtError::InvalidMessage("missing query args".into()))?;

        let sender_id = args
            .get(b"id".as_slice())
            .and_then(|v| v.as_bytes())
            .and_then(|b| NodeId::from_bytes(b).ok());

        let query = match query_name {
            "ping" => DhtQuery::Ping,
            "find_node" => {
                let target = args
                    .get(b"target".as_slice())
                    .and_then(|v| v.as_bytes())
                    .and_then(|b| NodeId::from_bytes(b).ok())
                    .ok_or_else(|| DhtError::InvalidMessage("missing target".into()))?;
                DhtQuery::FindNode { target }
            }
            "get_peers" => {
                let info_hash = args
                    .get(b"info_hash".as_slice())
                    .and_then(|v| v.as_bytes())
                    .filter(|b| b.len() == 20)
                    .map(|b| {
                        let mut hash = [0u8; 20];
                        hash.copy_from_slice(b);
                        hash
                    })
                    .ok_or_else(|| DhtError::InvalidMessage("missing info_hash".into()))?;
                DhtQuery::GetPeers { info_hash }
            }
            "announce_peer" => {
                let info_hash = args
                    .get(b"info_hash".as_slice())
                    .and_then(|v| v.as_bytes())
                    .filter(|b| b.len() == 20)
                    .map(|b| {
                        let mut hash = [0u8; 20];
                        hash.copy_from_slice(b);
                        hash
                    })
                    .ok_or_else(|| DhtError::InvalidMessage("missing info_hash".into()))?;

                let port = args
                    .get(b"port".as_slice())
                    .and_then(|v| v.as_integer())
                    .ok_or_else(|| DhtError::InvalidMessage("missing port".into()))?
                    as u16;

                let token = args
                    .get(b"token".as_slice())
                    .and_then(|v| v.as_bytes())
                    .cloned()
                    .ok_or_else(|| DhtError::InvalidMessage("missing token".into()))?;

                let implied_port = args
                    .get(b"implied_port".as_slice())
                    .and_then(|v| v.as_integer())
                    .map(|v| v == 1)
                    .unwrap_or(false);

                DhtQuery::AnnouncePeer {
                    info_hash,
                    port,
                    token,
                    implied_port,
                }
            }
            _ => {
                return Err(DhtError::InvalidMessage(format!(
                    "unknown query: {}",
                    query_name
                )))
            }
        };

        Ok(Self {
            transaction_id,
            sender_id,
            query: Some((query_name.to_string(), query)),
            response: None,
            ip: None,
        })
    }

    fn parse_response(
        transaction_id: TransactionId,
        dict: &BTreeMap<Bytes, Value>,
    ) -> Result<Self, DhtError> {
        // BEP-42: Extract top-level "ip" field
        let ip = dict
            .get(b"ip".as_slice())
            .and_then(|v| v.as_bytes())
            .cloned();
        let resp = dict
            .get(b"r".as_slice())
            .and_then(|v| v.as_dict())
            .ok_or_else(|| DhtError::InvalidMessage("missing response dict".into()))?;

        let sender_id = resp
            .get(b"id".as_slice())
            .and_then(|v| v.as_bytes())
            .and_then(|b| NodeId::from_bytes(b).ok())
            .ok_or_else(|| DhtError::InvalidMessage("missing id in response".into()))?;

        let nodes = resp
            .get(b"nodes".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|data| {
                data.chunks_exact(26)
                    .filter_map(Node::from_compact)
                    .collect()
            });

        let peers = resp
            .get(b"values".as_slice())
            .and_then(|v| v.as_list())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_bytes())
                    .filter(|b| b.len() == 6)
                    .map(|b| {
                        let ip = std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]);
                        let port = u16::from_be_bytes([b[4], b[5]]);
                        SocketAddr::new(std::net::IpAddr::V4(ip), port)
                    })
                    .collect()
            });

        let token = resp
            .get(b"token".as_slice())
            .and_then(|v| v.as_bytes())
            .cloned();

        let response = if peers.is_some() {
            DhtResponse::GetPeers {
                id: sender_id,
                token: token.unwrap_or_default(),
                peers,
                nodes,
            }
        } else if let Some(ref t) = token {
            DhtResponse::GetPeers {
                id: sender_id,
                token: t.clone(),
                peers: None,
                nodes,
            }
        } else if let Some(nodes) = nodes {
            DhtResponse::FindNode {
                id: sender_id,
                nodes,
            }
        } else {
            DhtResponse::Ping { id: sender_id }
        };

        Ok(Self {
            transaction_id,
            sender_id: Some(sender_id),
            query: None,
            response: Some(response),
            ip,
        })
    }

    fn parse_error(
        transaction_id: TransactionId,
        dict: &BTreeMap<Bytes, Value>,
    ) -> Result<Self, DhtError> {
        let error = dict
            .get(b"e".as_slice())
            .and_then(|v| v.as_list())
            .ok_or_else(|| DhtError::InvalidMessage("missing error list".into()))?;

        let code = error.first().and_then(|v| v.as_integer()).unwrap_or(0);

        let message = error
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string();

        Ok(Self {
            transaction_id,
            sender_id: None,
            query: None,
            response: Some(DhtResponse::Error { code, message }),
            ip: None,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, DhtError> {
        let mut dict = BTreeMap::new();

        dict.insert(
            Bytes::from_static(b"t"),
            Value::Bytes(self.transaction_id.clone()),
        );

        if let Some((name, query)) = &self.query {
            dict.insert(Bytes::from_static(b"y"), Value::string("q"));
            dict.insert(Bytes::from_static(b"q"), Value::string(name));

            let mut args = BTreeMap::new();

            if let Some(id) = &self.sender_id {
                args.insert(
                    Bytes::from_static(b"id"),
                    Value::Bytes(Bytes::copy_from_slice(id.as_bytes())),
                );
            }

            match query {
                DhtQuery::Ping => {}
                DhtQuery::FindNode { target } => {
                    args.insert(
                        Bytes::from_static(b"target"),
                        Value::Bytes(Bytes::copy_from_slice(target.as_bytes())),
                    );
                }
                DhtQuery::GetPeers { info_hash } => {
                    args.insert(
                        Bytes::from_static(b"info_hash"),
                        Value::Bytes(Bytes::copy_from_slice(info_hash)),
                    );
                }
                DhtQuery::AnnouncePeer {
                    info_hash,
                    port,
                    token,
                    implied_port,
                } => {
                    args.insert(
                        Bytes::from_static(b"info_hash"),
                        Value::Bytes(Bytes::copy_from_slice(info_hash)),
                    );
                    args.insert(Bytes::from_static(b"port"), Value::Integer(*port as i64));
                    args.insert(Bytes::from_static(b"token"), Value::Bytes(token.clone()));
                    if *implied_port {
                        args.insert(Bytes::from_static(b"implied_port"), Value::Integer(1));
                    }
                }
            }

            dict.insert(Bytes::from_static(b"a"), Value::Dict(args));
        }

        if let Some(response) = &self.response {
            match response {
                DhtResponse::Error { code, message } => {
                    dict.insert(Bytes::from_static(b"y"), Value::string("e"));
                    dict.insert(
                        Bytes::from_static(b"e"),
                        Value::List(vec![Value::Integer(*code), Value::string(message)]),
                    );
                }
                _ => {
                    dict.insert(Bytes::from_static(b"y"), Value::string("r"));

                    let mut resp = BTreeMap::new();

                    match response {
                        DhtResponse::Ping { id } => {
                            resp.insert(
                                Bytes::from_static(b"id"),
                                Value::Bytes(Bytes::copy_from_slice(id.as_bytes())),
                            );
                        }
                        DhtResponse::FindNode { id, nodes } => {
                            resp.insert(
                                Bytes::from_static(b"id"),
                                Value::Bytes(Bytes::copy_from_slice(id.as_bytes())),
                            );

                            let compact: Vec<u8> = nodes
                                .iter()
                                .filter_map(|n| n.to_compact())
                                .flatten()
                                .collect();

                            resp.insert(
                                Bytes::from_static(b"nodes"),
                                Value::Bytes(Bytes::from(compact)),
                            );
                        }
                        DhtResponse::GetPeers {
                            id,
                            token,
                            peers,
                            nodes,
                        } => {
                            resp.insert(
                                Bytes::from_static(b"id"),
                                Value::Bytes(Bytes::copy_from_slice(id.as_bytes())),
                            );
                            resp.insert(Bytes::from_static(b"token"), Value::Bytes(token.clone()));

                            if let Some(peers) = peers {
                                let values: Vec<Value> = peers
                                    .iter()
                                    .filter_map(|addr| {
                                        if let SocketAddr::V4(v4) = addr {
                                            let mut data = [0u8; 6];
                                            data[..4].copy_from_slice(&v4.ip().octets());
                                            data[4..6].copy_from_slice(&v4.port().to_be_bytes());
                                            Some(Value::Bytes(Bytes::copy_from_slice(&data)))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                resp.insert(Bytes::from_static(b"values"), Value::List(values));
                            }

                            if let Some(nodes) = nodes {
                                let compact: Vec<u8> = nodes
                                    .iter()
                                    .filter_map(|n| n.to_compact())
                                    .flatten()
                                    .collect();
                                resp.insert(
                                    Bytes::from_static(b"nodes"),
                                    Value::Bytes(Bytes::from(compact)),
                                );
                            }
                        }
                        DhtResponse::AnnouncePeer { id } => {
                            resp.insert(
                                Bytes::from_static(b"id"),
                                Value::Bytes(Bytes::copy_from_slice(id.as_bytes())),
                            );
                        }
                        DhtResponse::Error { .. } => unreachable!(),
                    }

                    dict.insert(Bytes::from_static(b"r"), Value::Dict(resp));

                    // BEP-42: Include IP field at top level of response
                    if let Some(ref ip) = self.ip {
                        dict.insert(Bytes::from_static(b"ip"), Value::Bytes(ip.clone()));
                    }
                }
            }
        }

        encode(&Value::Dict(dict)).map_err(|_| DhtError::InvalidMessage("encode failed".into()))
    }
}
