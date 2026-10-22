use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use tokio::net::TcpStream;

use super::bitfield::Bitfield;
use super::choking::ChokingState;
use super::error::PeerError;
use super::extension::ExtensionHandshake;
use super::message::{validate_hash_request, Handshake, Message};
use super::peer_id::PeerId;
use super::transport::PeerTransport;

/// The connection state of a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// TCP connection in progress.
    Connecting,
    /// Connected, performing BitTorrent handshake.
    Handshaking,
    /// Fully connected and ready for data exchange.
    Connected,
    /// Connection has been closed.
    Disconnected,
}

/// The protocol version to use for this connection.
///
/// For hybrid torrents, we need to track which protocol version
/// to use with each peer based on their capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolMode {
    /// Use BitTorrent v1 protocol (SHA1 pieces, traditional piece indexing).
    #[default]
    V1,
    /// Use BitTorrent v2 protocol (SHA256 merkle, per-file pieces).
    V2,
}

/// A connection to a BitTorrent peer.
///
/// Manages the TCP connection and protocol state for communicating with a
/// single peer, including handshake, message exchange, and choking state.
///
/// # Examples
///
/// ```no_run
/// use rbit::peer::{PeerConnection, PeerId, Message};
/// use std::net::SocketAddr;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let addr: SocketAddr = "192.168.1.100:6881".parse()?;
/// let info_hash = [0u8; 20];
/// let peer_id = PeerId::generate();
///
/// let mut conn = PeerConnection::connect(addr, info_hash, *peer_id.as_bytes()).await?;
///
/// // Express interest and wait for unchoke
/// conn.send(Message::Interested).await?;
/// # Ok(())
/// # }
/// ```
pub struct PeerConnection {
    /// The peer's socket address.
    pub addr: SocketAddr,
    /// The peer's ID (if received in handshake).
    pub peer_id: Option<PeerId>,
    /// Current connection state.
    pub state: PeerState,
    /// Choking state for this connection.
    pub choking: ChokingState,
    /// The peer's bitfield (pieces they have).
    pub bitfield: Option<Bitfield>,
    /// Extension handshake data (if BEP-10 is supported).
    pub extension_handshake: Option<ExtensionHandshake>,
    /// Whether the peer supports the Fast Extension (BEP-6).
    pub supports_fast: bool,
    /// Whether the peer supports the Extension Protocol (BEP-10).
    pub supports_extension: bool,
    /// Whether the peer supports BitTorrent v2 (BEP-52).
    pub supports_v2: bool,
    /// The protocol mode to use for this connection.
    pub protocol_mode: ProtocolMode,
    /// When the connection was established.
    pub connected_at: Instant,
    /// When the last message was received.
    pub last_message_at: Instant,
    /// Total bytes downloaded from this peer.
    pub bytes_downloaded: u64,
    /// Total bytes uploaded to this peer.
    pub bytes_uploaded: u64,
    transport: Option<PeerTransport>,
}

impl PeerConnection {
    /// Connects to a peer using the v1 protocol.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: [u8; 20],
        our_peer_id: [u8; 20],
    ) -> Result<Self, PeerError> {
        Self::connect_with_mode(addr, info_hash, our_peer_id, false).await
    }

    /// Connects to a peer, optionally advertising v2 support.
    pub async fn connect_with_mode(
        addr: SocketAddr,
        info_hash: [u8; 20],
        our_peer_id: [u8; 20],
        advertise_v2: bool,
    ) -> Result<Self, PeerError> {
        let stream = TcpStream::connect(addr).await?;
        let mut transport = PeerTransport::new(stream);

        let handshake = if advertise_v2 {
            Handshake::new_v2(info_hash, our_peer_id)
        } else {
            Handshake::new(info_hash, our_peer_id)
        };
        transport.send_handshake(&handshake).await?;

        let their_handshake = transport.receive_handshake().await?;

        if their_handshake.info_hash != info_hash {
            return Err(PeerError::InfoHashMismatch);
        }

        let supports_v2 = their_handshake.supports_v2();
        // Use v2 protocol if both sides support it
        let protocol_mode = if advertise_v2 && supports_v2 {
            ProtocolMode::V2
        } else {
            ProtocolMode::V1
        };

        let now = Instant::now();
        Ok(Self {
            addr,
            peer_id: PeerId::from_bytes(&their_handshake.peer_id),
            state: PeerState::Connected,
            choking: ChokingState::default(),
            bitfield: None,
            extension_handshake: None,
            supports_fast: their_handshake.supports_fast_extension(),
            supports_extension: their_handshake.supports_extension_protocol(),
            supports_v2,
            protocol_mode,
            connected_at: now,
            last_message_at: now,
            bytes_downloaded: 0,
            bytes_uploaded: 0,
            transport: Some(transport),
        })
    }

    /// Accepts a connection from a peer using the v1 protocol.
    pub async fn accept(
        stream: TcpStream,
        info_hash: [u8; 20],
        our_peer_id: [u8; 20],
    ) -> Result<Self, PeerError> {
        Self::accept_with_mode(stream, info_hash, our_peer_id, false).await
    }

    /// Accepts a connection from a peer, optionally advertising v2 support.
    pub async fn accept_with_mode(
        stream: TcpStream,
        info_hash: [u8; 20],
        our_peer_id: [u8; 20],
        advertise_v2: bool,
    ) -> Result<Self, PeerError> {
        let addr = stream.peer_addr()?;
        let mut transport = PeerTransport::new(stream);

        let their_handshake = transport.receive_handshake().await?;

        if their_handshake.info_hash != info_hash {
            return Err(PeerError::InfoHashMismatch);
        }

        let handshake = if advertise_v2 {
            Handshake::new_v2(info_hash, our_peer_id)
        } else {
            Handshake::new(info_hash, our_peer_id)
        };
        transport.send_handshake(&handshake).await?;

        let supports_v2 = their_handshake.supports_v2();
        // Use v2 protocol if both sides support it
        let protocol_mode = if advertise_v2 && supports_v2 {
            ProtocolMode::V2
        } else {
            ProtocolMode::V1
        };

        let now = Instant::now();
        Ok(Self {
            addr,
            peer_id: PeerId::from_bytes(&their_handshake.peer_id),
            state: PeerState::Connected,
            choking: ChokingState::default(),
            bitfield: None,
            extension_handshake: None,
            supports_fast: their_handshake.supports_fast_extension(),
            supports_extension: their_handshake.supports_extension_protocol(),
            supports_v2,
            protocol_mode,
            connected_at: now,
            last_message_at: now,
            bytes_downloaded: 0,
            bytes_uploaded: 0,
            transport: Some(transport),
        })
    }

    pub async fn send(&mut self, message: Message) -> Result<(), PeerError> {
        if let Some(ref mut transport) = self.transport {
            transport.send_message(&message).await?;

            if let Message::Piece { ref data, .. } = message {
                self.bytes_uploaded += data.len() as u64;
            }

            Ok(())
        } else {
            Err(PeerError::ConnectionClosed)
        }
    }

    pub async fn receive(&mut self) -> Result<Message, PeerError> {
        if let Some(ref mut transport) = self.transport {
            let message = transport.receive_message().await?;
            self.last_message_at = Instant::now();

            match &message {
                Message::Choke => self.choking.peer_choking = true,
                Message::Unchoke => self.choking.peer_choking = false,
                Message::Interested => self.choking.peer_interested = true,
                Message::NotInterested => self.choking.peer_interested = false,
                Message::Piece { data, .. } => {
                    self.bytes_downloaded += data.len() as u64;
                }
                _ => {}
            }

            Ok(message)
        } else {
            Err(PeerError::ConnectionClosed)
        }
    }

    pub fn disconnect(&mut self) {
        self.transport = None;
        self.state = PeerState::Disconnected;
    }

    pub fn is_connected(&self) -> bool {
        self.state == PeerState::Connected && self.transport.is_some()
    }

    pub fn can_request(&self) -> bool {
        self.is_connected() && !self.choking.peer_choking && self.choking.am_interested
    }

    pub fn set_interested(&mut self, interested: bool) {
        self.choking.am_interested = interested;
    }

    pub fn set_choking(&mut self, choking: bool) {
        self.choking.am_choking = choking;
    }

    /// Returns true if using v2 protocol mode with this peer.
    pub fn is_v2_mode(&self) -> bool {
        self.protocol_mode == ProtocolMode::V2
    }

    /// Returns true if using v1 protocol mode with this peer.
    pub fn is_v1_mode(&self) -> bool {
        self.protocol_mode == ProtocolMode::V1
    }

    /// Upgrades the protocol mode to v2 if the peer supports it.
    ///
    /// This should only be called after confirming both sides support v2.
    pub fn upgrade_to_v2(&mut self) {
        if self.supports_v2 {
            self.protocol_mode = ProtocolMode::V2;
        }
    }

    /// Forces protocol mode (use with caution).
    pub fn set_protocol_mode(&mut self, mode: ProtocolMode) {
        self.protocol_mode = mode;
    }

    /// Returns true if we can use v2 hash messages with this peer.
    pub fn can_use_hash_messages(&self) -> bool {
        self.supports_v2 && self.protocol_mode == ProtocolMode::V2
    }

    // =========================================================================
    // BitTorrent v2 Hash Message Methods (BEP-52)
    // =========================================================================

    /// Sends a HashRequest message to request merkle tree hashes.
    ///
    /// # Arguments
    ///
    /// * `pieces_root` - The 32-byte merkle root of the file
    /// * `base_layer` - Tree layer to request (0 = leaf/piece hashes)
    /// * `index` - Starting index in the layer (must be multiple of `length`)
    /// * `length` - Number of hashes to request (must be power of 2, >= 2, <= 512)
    /// * `proof_layers` - Number of uncle hash layers to include for verification
    ///
    /// # Errors
    ///
    /// Returns an error if the request parameters are invalid per BEP-52 or
    /// if the connection doesn't support v2.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use rbit::peer::PeerConnection;
    /// # async fn example(conn: &mut PeerConnection) -> Result<(), Box<dyn std::error::Error>> {
    /// let pieces_root = [0xABu8; 32];
    /// // Request 4 hashes from layer 0 starting at index 0, with 2 proof layers
    /// conn.send_hash_request(pieces_root, 0, 0, 4, 2).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_hash_request(
        &mut self,
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    ) -> Result<(), PeerError> {
        if !self.can_use_hash_messages() {
            return Err(PeerError::Protocol("v2 hash messages not supported".into()));
        }

        // Validate request parameters per BEP-52
        if let Some(err) = validate_hash_request(length, index) {
            return Err(PeerError::InvalidMessage(err.into()));
        }

        let message = Message::HashRequest {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
        };

        self.send(message).await
    }

    /// Sends a Hashes response message with merkle tree hashes.
    ///
    /// # Arguments
    ///
    /// * `pieces_root` - The 32-byte merkle root of the file
    /// * `base_layer` - Tree layer the hashes are from (0 = leaf/piece hashes)
    /// * `index` - Starting index in the layer
    /// * `length` - Number of layer hashes (not including proof hashes)
    /// * `proof_layers` - Number of uncle hash layers included
    /// * `hashes` - Concatenated 32-byte hashes (length + proof_layers total)
    ///
    /// # Errors
    ///
    /// Returns an error if the connection doesn't support v2 or is closed.
    pub async fn send_hashes(
        &mut self,
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
        hashes: Bytes,
    ) -> Result<(), PeerError> {
        if !self.can_use_hash_messages() {
            return Err(PeerError::Protocol("v2 hash messages not supported".into()));
        }

        // Validate hash data length
        let expected_len = ((length + proof_layers) as usize) * 32;
        if hashes.len() != expected_len {
            return Err(PeerError::InvalidMessage(format!(
                "hash data length {} doesn't match expected {}",
                hashes.len(),
                expected_len
            )));
        }

        let message = Message::Hashes {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
            hashes,
        };

        self.send(message).await
    }

    /// Sends a HashReject message to reject a hash request.
    ///
    /// This is sent when we cannot or will not service a hash request,
    /// for example if we don't have the requested file's merkle tree.
    ///
    /// # Arguments
    ///
    /// * `pieces_root` - The 32-byte merkle root from the original request
    /// * `base_layer` - Tree layer from the original request
    /// * `index` - Starting index from the original request
    /// * `length` - Length from the original request
    /// * `proof_layers` - Proof layers from the original request
    pub async fn send_hash_reject(
        &mut self,
        pieces_root: [u8; 32],
        base_layer: u32,
        index: u32,
        length: u32,
        proof_layers: u32,
    ) -> Result<(), PeerError> {
        if !self.can_use_hash_messages() {
            return Err(PeerError::Protocol("v2 hash messages not supported".into()));
        }

        let message = Message::HashReject {
            pieces_root,
            base_layer,
            index,
            length,
            proof_layers,
        };

        self.send(message).await
    }

    /// Checks if a received message is a v2 hash-related message.
    pub fn is_hash_message(message: &Message) -> bool {
        matches!(
            message,
            Message::HashRequest { .. } | Message::Hashes { .. } | Message::HashReject { .. }
        )
    }
}
