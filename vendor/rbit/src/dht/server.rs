use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::RwLock;
use sha1::{Digest, Sha1};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::error::DhtError;
use super::message::{DhtMessage, DhtQuery, DhtResponse};
use super::node::{Node, NodeId};
use super::node_id_security::{
    decode_compact_ip_port, encode_compact_ip_port, generate_secure_node_id, is_local_network,
    validate_node_id, BEP42_REQUIRED_VOTES,
};
use super::routing::RoutingTable;

const DHT_QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PENDING_QUERIES: usize = 100;
const DHT_ALPHA: usize = 3;
const DHT_MAX_ITERATIONS: usize = 10;
const MAX_PEERS_PER_TORRENT: usize = 1000;
const PEER_ANNOUNCE_LIFETIME: Duration = Duration::from_secs(30 * 60);

pub const BOOTSTRAP_NODES: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

struct AnnouncedPeer {
    addr: SocketAddr,
    announced_at: Instant,
}

struct PeerStore {
    peers: HashMap<[u8; 20], Vec<AnnouncedPeer>>,
}

impl PeerStore {
    fn new() -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    fn add_peer(&mut self, info_hash: [u8; 20], addr: SocketAddr) {
        let peers = self.peers.entry(info_hash).or_default();

        let now = Instant::now();
        peers.retain(|p| now.duration_since(p.announced_at) < PEER_ANNOUNCE_LIFETIME);
        peers.retain(|p| p.addr != addr);

        if peers.len() < MAX_PEERS_PER_TORRENT {
            peers.push(AnnouncedPeer {
                addr,
                announced_at: now,
            });
        }
    }

    fn get_peers(&mut self, info_hash: &[u8; 20]) -> Vec<SocketAddr> {
        let now = Instant::now();
        if let Some(peers) = self.peers.get_mut(info_hash) {
            peers.retain(|p| now.duration_since(p.announced_at) < PEER_ANNOUNCE_LIFETIME);
            peers.iter().map(|p| p.addr).collect()
        } else {
            Vec::new()
        }
    }
}

struct PendingQuery {
    sender: mpsc::Sender<DhtResponse>,
}

struct TokenSecrets {
    current: [u8; 16],
    previous: [u8; 16],
}

impl TokenSecrets {
    fn new() -> Self {
        Self {
            current: rand::random(),
            previous: rand::random(),
        }
    }

    fn rotate(&mut self) {
        self.previous = self.current;
        self.current = rand::random();
    }
}

/// Configuration for BEP-42 DHT security extension.
#[derive(Debug, Clone, Default)]
pub struct Bep42Config {
    /// Whether to enforce BEP-42 node ID validation.
    /// When enabled, nodes with invalid IDs will be rejected.
    pub enforce: bool,
    /// External IP address (if known). If None, will be discovered.
    pub external_ip: Option<IpAddr>,
}

/// The main DHT server for peer discovery.
///
/// `DhtServer` implements the BitTorrent DHT protocol ([BEP-5]) for finding
/// peers without relying on trackers. Also supports BEP-42 for DHT security.
///
/// # Examples
///
/// ```no_run
/// use rbit::dht::DhtServer;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Bind to a UDP port
/// let dht = DhtServer::bind(6881).await?;
///
/// // Bootstrap from known nodes
/// dht.bootstrap().await?;
///
/// // Find peers for a torrent
/// let info_hash = [0u8; 20];
/// let peers = dht.get_peers(info_hash).await?;
/// # Ok(())
/// # }
/// ```
///
/// [BEP-5]: http://bittorrent.org/beps/bep_0005.html
pub struct DhtServer {
    socket: Arc<UdpSocket>,
    our_id: RwLock<NodeId>,
    routing_table: Arc<RoutingTable>,
    pending_queries: Arc<RwLock<HashMap<Bytes, PendingQuery>>>,
    port: u16,
    token_secrets: RwLock<TokenSecrets>,
    peer_store: RwLock<PeerStore>,
    /// BEP-42: External IP address votes from other nodes
    external_ip_votes: RwLock<HashMap<IpAddr, u32>>,
    /// BEP-42: Confirmed external IP address
    external_ip: RwLock<Option<IpAddr>>,
    /// BEP-42: Configuration
    bep42_config: Bep42Config,
}

impl DhtServer {
    /// Binds a DHT server to the specified port with default configuration.
    pub async fn bind(port: u16) -> Result<Self, DhtError> {
        Self::bind_with_config(port, Bep42Config::default()).await
    }

    /// Binds a DHT server with BEP-42 configuration.
    ///
    /// If `config.external_ip` is provided, a BEP-42 compliant node ID will be generated.
    /// Otherwise, a random node ID is used until external IP is discovered.
    pub async fn bind_with_config(port: u16, config: Bep42Config) -> Result<Self, DhtError> {
        let socket = UdpSocket::bind(format!("0.0.0.0:{}", port)).await?;
        let local_addr = socket.local_addr()?;

        // Generate node ID based on external IP if known
        let our_id = if let Some(ip) = config.external_ip {
            generate_secure_node_id(ip)
        } else {
            NodeId::generate()
        };

        info!("DHT server bound to {} with id {}", local_addr, our_id);

        Ok(Self {
            socket: Arc::new(socket),
            our_id: RwLock::new(our_id),
            routing_table: Arc::new(RoutingTable::new(our_id)),
            pending_queries: Arc::new(RwLock::new(HashMap::new())),
            port: local_addr.port(),
            token_secrets: RwLock::new(TokenSecrets::new()),
            peer_store: RwLock::new(PeerStore::new()),
            external_ip_votes: RwLock::new(HashMap::new()),
            external_ip: RwLock::new(config.external_ip),
            bep42_config: config,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Returns our current node ID.
    pub fn our_id(&self) -> NodeId {
        *self.our_id.read()
    }

    /// Returns the confirmed external IP address (if known).
    pub fn external_ip(&self) -> Option<IpAddr> {
        *self.external_ip.read()
    }

    /// Returns whether BEP-42 enforcement is enabled.
    pub fn is_bep42_enforced(&self) -> bool {
        self.bep42_config.enforce
    }

    /// Processes an external IP vote from a DHT response.
    /// When enough votes agree, regenerates our node ID to be BEP-42 compliant.
    fn process_external_ip_vote(&self, ip: IpAddr) {
        // Skip if we already have a confirmed external IP
        if self.external_ip.read().is_some() {
            return;
        }

        let mut votes = self.external_ip_votes.write();
        let count = votes.entry(ip).or_insert(0);
        *count += 1;

        if *count >= BEP42_REQUIRED_VOTES {
            // We have consensus on our external IP
            info!(
                "BEP-42: External IP confirmed as {} after {} votes",
                ip, count
            );

            // Regenerate node ID based on confirmed IP
            let new_id = generate_secure_node_id(ip);
            *self.our_id.write() = new_id;
            *self.external_ip.write() = Some(ip);

            info!("BEP-42: Node ID regenerated to {}", new_id);
        }
    }

    pub fn routing_table(&self) -> &RoutingTable {
        &self.routing_table
    }

    pub async fn bootstrap(&self) -> Result<(), DhtError> {
        info!("Starting DHT bootstrap");

        for addr_str in BOOTSTRAP_NODES {
            match tokio::net::lookup_host(addr_str).await {
                Ok(mut addrs) => {
                    if let Some(addr) = addrs.next() {
                        debug!("Pinging bootstrap node {}", addr);
                        if let Ok(response) = self.ping(addr).await {
                            info!("Got response from bootstrap node: {:?}", response);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to resolve bootstrap node {}: {}", addr_str, e);
                }
            }
        }

        self.find_node(self.our_id()).await?;

        info!(
            "DHT bootstrap complete, {} nodes in routing table",
            self.routing_table.node_count()
        );
        Ok(())
    }

    pub async fn ping(&self, addr: SocketAddr) -> Result<DhtResponse, DhtError> {
        let tid = self.generate_transaction_id();
        let our_id = self.our_id();
        let msg = DhtMessage::ping(tid.clone(), &our_id);

        self.send_query(addr, msg, tid).await
    }

    pub async fn find_node(&self, target: NodeId) -> Result<Vec<Node>, DhtError> {
        let closest = self.routing_table.find_closest(&target, 8);

        if closest.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_nodes = Vec::new();

        let nodes_to_query: Vec<_> = closest.iter().take(DHT_ALPHA).cloned().collect();
        let mut queries = Vec::with_capacity(nodes_to_query.len());
        let our_id = self.our_id();

        for node in &nodes_to_query {
            let tid = self.generate_transaction_id();
            let msg = DhtMessage::find_node(tid.clone(), &our_id, target);
            queries.push((node.id, self.send_query(node.addr, msg, tid)));
        }

        let results = futures::future::join_all(
            queries
                .into_iter()
                .map(|(id, fut)| async move { (id, fut.await) }),
        )
        .await;

        for (node_id, result) in results {
            match result {
                Ok(DhtResponse::FindNode { nodes, .. }) => {
                    for n in nodes {
                        self.routing_table.add_node(n.clone());
                        all_nodes.push(n);
                    }
                }
                Ok(DhtResponse::GetPeers {
                    nodes: Some(nodes), ..
                }) => {
                    for n in nodes {
                        self.routing_table.add_node(n.clone());
                        all_nodes.push(n);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    debug!("find_node query failed: {}", e);
                    self.routing_table.mark_failed(&node_id);
                }
            }
        }

        Ok(all_nodes)
    }

    pub async fn get_peers(&self, info_hash: [u8; 20]) -> Result<Vec<SocketAddr>, DhtError> {
        let target = NodeId::from_bytes(&info_hash)?;

        let mut peers = Vec::new();
        let mut queried = std::collections::HashSet::new();
        let mut to_query: Vec<Node> = self.routing_table.find_closest(&target, 8);

        for _ in 0..DHT_MAX_ITERATIONS {
            if to_query.is_empty() {
                break;
            }

            to_query.sort_by(|a, b| {
                let dist_a = a.id.distance(&target);
                let dist_b = b.id.distance(&target);
                dist_a.cmp(&dist_b)
            });

            let mut queries = Vec::new();
            let mut nodes_to_query = Vec::new();

            for node in to_query.iter() {
                if !queried.contains(&node.id) && queries.len() < DHT_ALPHA {
                    queried.insert(node.id);
                    nodes_to_query.push(node.clone());

                    let tid = self.generate_transaction_id();
                    let our_id = self.our_id();
                    let msg = DhtMessage::get_peers(tid.clone(), &our_id, info_hash);
                    queries.push(self.send_query(node.addr, msg, tid));
                }
            }

            if queries.is_empty() {
                break;
            }

            let results = futures::future::join_all(queries).await;
            let mut new_nodes = Vec::new();

            for (i, result) in results.into_iter().enumerate() {
                match result {
                    Ok(DhtResponse::GetPeers {
                        peers: Some(p),
                        nodes,
                        ..
                    }) => {
                        peers.extend(p);
                        if let Some(nodes) = nodes {
                            for n in nodes {
                                if !queried.contains(&n.id) {
                                    self.routing_table.add_node(n.clone());
                                    new_nodes.push(n);
                                }
                            }
                        }
                    }
                    Ok(DhtResponse::GetPeers {
                        nodes: Some(nodes), ..
                    }) => {
                        for n in nodes {
                            if !queried.contains(&n.id) {
                                self.routing_table.add_node(n.clone());
                                new_nodes.push(n);
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        debug!("get_peers query failed: {}", e);
                        if i < nodes_to_query.len() {
                            self.routing_table.mark_failed(&nodes_to_query[i].id);
                        }
                    }
                }
            }

            to_query = new_nodes;

            if peers.len() >= 50 {
                break;
            }
        }

        info!(
            "DHT get_peers found {} peers after querying {} nodes",
            peers.len(),
            queried.len()
        );
        Ok(peers)
    }

    async fn send_query(
        &self,
        addr: SocketAddr,
        msg: DhtMessage,
        tid: Bytes,
    ) -> Result<DhtResponse, DhtError> {
        let (tx, mut rx) = mpsc::channel(1);

        {
            let mut pending = self.pending_queries.write();
            if pending.len() >= MAX_PENDING_QUERIES {
                return Err(DhtError::RateLimited);
            }

            pending.insert(tid.clone(), PendingQuery { sender: tx });
        }

        let data = msg.encode()?;
        self.socket.send_to(&data, addr).await?;

        let result = timeout(DHT_QUERY_TIMEOUT, rx.recv()).await;

        {
            self.pending_queries.write().remove(&tid);
        }

        match result {
            Ok(Some(response)) => Ok(response),
            Ok(None) => Err(DhtError::Timeout),
            Err(_) => Err(DhtError::Timeout),
        }
    }

    pub async fn run(&self) -> Result<(), DhtError> {
        let mut buf = vec![0u8; 65535];
        let mut refresh_interval = tokio::time::interval(Duration::from_secs(15 * 60));
        let mut token_rotation_interval = tokio::time::interval(Duration::from_secs(5 * 60));

        refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        token_rotation_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        refresh_interval.tick().await;
        token_rotation_interval.tick().await;

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                    let (n, addr) = result?;
                    match DhtMessage::parse(&buf[..n]) {
                        Ok(msg) => {
                            self.handle_message(msg, addr).await;
                        }
                        Err(e) => {
                            debug!("Failed to parse DHT message from {}: {}", addr, e);
                        }
                    }
                }
                _ = refresh_interval.tick() => {
                    self.refresh_stale_buckets().await;
                }
                _ = token_rotation_interval.tick() => {
                    self.rotate_token_secret();
                }
            }
        }
    }

    async fn refresh_stale_buckets(&self) {
        let stale = self.routing_table.stale_buckets();
        if stale.is_empty() {
            return;
        }

        debug!("Refreshing {} stale DHT buckets", stale.len());

        for bucket_idx in stale {
            let target = self.generate_id_for_bucket(bucket_idx);
            let _ = self.find_node(target).await;
        }
    }

    fn generate_id_for_bucket(&self, bucket_idx: usize) -> NodeId {
        if bucket_idx >= 160 {
            return NodeId::generate();
        }

        let mut id = self.our_id().0;
        let byte_idx = bucket_idx / 8;
        let bit_idx = 7 - (bucket_idx % 8);

        id[byte_idx] ^= 1 << bit_idx;

        if bit_idx > 0 {
            let random_byte: u8 = rand::random();
            let keep_mask = !((1u8 << bit_idx) - 1);
            let random_mask = (1u8 << bit_idx) - 1;
            id[byte_idx] = (id[byte_idx] & keep_mask) | (random_byte & random_mask);
        }

        for byte in id.iter_mut().skip(byte_idx + 1) {
            *byte = rand::random();
        }

        NodeId(id)
    }

    async fn handle_message(&self, msg: DhtMessage, addr: SocketAddr) {
        // BEP-42: Validate node ID if enforcement is enabled
        if let Some(id) = msg.sender_id {
            let should_add = if self.bep42_config.enforce && !is_local_network(&addr.ip()) {
                validate_node_id(&id, addr.ip())
            } else {
                true
            };

            if should_add {
                self.routing_table.add_node(Node::new(id, addr));
            } else {
                debug!(
                    "BEP-42: Rejecting node {} from {} - invalid node ID",
                    id, addr
                );
            }
        }

        if let Some(response) = msg.response.clone() {
            // BEP-42: Process "ip" field from responses for external IP discovery
            if let Some(ref ip_data) = msg.ip {
                if let Some((ip, _port)) = decode_compact_ip_port(ip_data) {
                    self.process_external_ip_vote(ip);
                }
            }

            let pending = self.pending_queries.read();
            if let Some(query) = pending.get(&msg.transaction_id) {
                let _ = query.sender.try_send(response);
            }
            return;
        }

        if let Some((name, query)) = msg.query {
            self.handle_query(msg.transaction_id, addr, &name, query, msg.sender_id)
                .await;
        }
    }

    async fn handle_query(
        &self,
        tid: Bytes,
        addr: SocketAddr,
        name: &str,
        query: DhtQuery,
        _sender_id: Option<NodeId>,
    ) {
        let our_id = self.our_id();
        // BEP-42: Include requestor's IP in response
        let requester_ip = Bytes::from(encode_compact_ip_port(addr.ip(), addr.port()));

        let response = match (name, query) {
            ("ping", _) => DhtMessage {
                transaction_id: tid,
                sender_id: None,
                query: None,
                response: Some(DhtResponse::Ping { id: our_id }),
                ip: Some(requester_ip),
            },
            ("find_node", DhtQuery::FindNode { target }) => {
                let nodes = self.routing_table.find_closest(&target, 8);
                DhtMessage {
                    transaction_id: tid,
                    sender_id: None,
                    query: None,
                    response: Some(DhtResponse::FindNode { id: our_id, nodes }),
                    ip: Some(requester_ip),
                }
            }
            ("get_peers", DhtQuery::GetPeers { info_hash }) => {
                let target = NodeId::from_bytes(&info_hash).unwrap_or(our_id);
                let nodes = self.routing_table.find_closest(&target, 8);
                let token = self.generate_token(&addr);

                let stored_peers = self.peer_store.write().get_peers(&info_hash);
                let peers = if stored_peers.is_empty() {
                    None
                } else {
                    Some(stored_peers)
                };

                DhtMessage {
                    transaction_id: tid,
                    sender_id: None,
                    query: None,
                    response: Some(DhtResponse::GetPeers {
                        id: our_id,
                        token,
                        peers,
                        nodes: Some(nodes),
                    }),
                    ip: Some(requester_ip),
                }
            }
            (
                "announce_peer",
                DhtQuery::AnnouncePeer {
                    info_hash,
                    port,
                    implied_port,
                    token,
                },
            ) => {
                if !self.validate_token(&addr, &token) {
                    debug!("Rejecting announce_peer from {} - invalid token", addr);
                    DhtMessage {
                        transaction_id: tid,
                        sender_id: None,
                        query: None,
                        response: Some(DhtResponse::Error {
                            code: 203,
                            message: "Invalid token".to_string(),
                        }),
                        ip: Some(requester_ip),
                    }
                } else {
                    let peer_port = if implied_port { addr.port() } else { port };
                    let peer_addr = SocketAddr::new(addr.ip(), peer_port);
                    self.peer_store.write().add_peer(info_hash, peer_addr);
                    debug!(
                        "Stored announced peer {} for info_hash {:?}",
                        peer_addr,
                        &info_hash[..8]
                    );

                    DhtMessage {
                        transaction_id: tid,
                        sender_id: None,
                        query: None,
                        response: Some(DhtResponse::AnnouncePeer { id: our_id }),
                        ip: Some(requester_ip),
                    }
                }
            }
            _ => return,
        };

        if let Ok(data) = response.encode() {
            let _ = self.socket.send_to(&data, addr).await;
        }
    }

    fn generate_transaction_id(&self) -> Bytes {
        let id: [u8; 2] = rand::random();
        Bytes::copy_from_slice(&id)
    }

    fn generate_token(&self, addr: &SocketAddr) -> Bytes {
        self.generate_token_with_secret(addr, &self.token_secrets.read().current)
    }

    fn generate_token_with_secret(&self, addr: &SocketAddr, secret: &[u8; 16]) -> Bytes {
        let mut hasher = Sha1::new();
        hasher.update(secret);
        hasher.update(addr.ip().to_string().as_bytes());

        let result = hasher.finalize();
        Bytes::copy_from_slice(&result[..8])
    }

    fn validate_token(&self, addr: &SocketAddr, token: &Bytes) -> bool {
        let secrets = self.token_secrets.read();

        let current_token = self.generate_token_with_secret(addr, &secrets.current);
        if current_token == *token {
            return true;
        }

        let previous_token = self.generate_token_with_secret(addr, &secrets.previous);
        previous_token == *token
    }

    pub fn rotate_token_secret(&self) {
        self.token_secrets.write().rotate();
        debug!("DHT token secret rotated");
    }
}
