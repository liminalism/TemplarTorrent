use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::error::PeerError;
use super::message::{Handshake, Message, HANDSHAKE_LEN};

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Any byte stream a peer connection can run over: a `TcpStream`, an
/// [`super::MseStream`] after the encryption handshake, or a test pipe.
pub trait PeerStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> PeerStream for T {}

/// The type-erased stream [`super::PeerConnection`] owns.
pub type BoxedPeerStream = Box<dyn PeerStream>;

/// Peer messages are small and pipelined. With Nagle on, a Request waits for
/// the ACK of the one before it, which the remote delays by up to 40 ms on
/// Linux. Failure leaves the default; not fatal.
pub(crate) fn set_peer_nodelay(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}

pub struct PeerTransport<S = TcpStream> {
    stream: S,
    read_buf: BytesMut,
}

impl PeerTransport<TcpStream> {
    pub fn new(stream: TcpStream) -> Self {
        set_peer_nodelay(&stream);
        Self::from_stream(stream)
    }

    pub fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> PeerTransport<S> {
    /// A transport over an already-connected stream. Socket options are the
    /// caller's: set `TCP_NODELAY` on the TCP socket underneath.
    pub fn from_stream(stream: S) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(32 * 1024),
        }
    }

    pub async fn send_handshake(&mut self, handshake: &Handshake) -> Result<(), PeerError> {
        let data = handshake.encode();
        self.write_frame(&data).await
    }

    pub async fn receive_handshake(&mut self) -> Result<Handshake, PeerError> {
        while self.read_buf.len() < HANDSHAKE_LEN {
            let n = timeout(READ_TIMEOUT, self.stream.read_buf(&mut self.read_buf))
                .await
                .map_err(|_| PeerError::Timeout)??;

            if n == 0 {
                return Err(PeerError::ConnectionClosed);
            }
        }

        let data = self.read_buf.split_to(HANDSHAKE_LEN);
        Handshake::decode(&data)
    }

    pub async fn send_message(&mut self, message: &Message) -> Result<(), PeerError> {
        let data = message.encode();
        self.write_frame(&data).await
    }

    /// Write one frame and flush it. Flushing is a no-op on a `TcpStream`;
    /// an encrypting stream drains its buffer there.
    async fn write_frame(&mut self, data: &[u8]) -> Result<(), PeerError> {
        timeout(WRITE_TIMEOUT, async {
            self.stream.write_all(data).await?;
            self.stream.flush().await
        })
        .await
        .map_err(|_| PeerError::Timeout)??;
        Ok(())
    }

    pub async fn receive_message(&mut self) -> Result<Message, PeerError> {
        while self.read_buf.len() < 4 {
            let n = timeout(READ_TIMEOUT, self.stream.read_buf(&mut self.read_buf))
                .await
                .map_err(|_| PeerError::Timeout)??;

            if n == 0 {
                return Err(PeerError::ConnectionClosed);
            }
        }

        let length = u32::from_be_bytes([
            self.read_buf[0],
            self.read_buf[1],
            self.read_buf[2],
            self.read_buf[3],
        ]) as usize;

        if length > MAX_MESSAGE_SIZE {
            return Err(PeerError::InvalidMessage(format!(
                "message too large: {}",
                length
            )));
        }

        let total_len = 4 + length;
        while self.read_buf.len() < total_len {
            let n = timeout(READ_TIMEOUT, self.stream.read_buf(&mut self.read_buf))
                .await
                .map_err(|_| PeerError::Timeout)??;

            if n == 0 {
                return Err(PeerError::ConnectionClosed);
            }
        }

        let data = self.read_buf.split_to(total_len);
        Message::decode(data.freeze())
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}
