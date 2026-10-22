use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::error::PeerError;
use super::message::{Handshake, Message, HANDSHAKE_LEN};

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PeerTransport {
    stream: TcpStream,
    read_buf: BytesMut,
}

impl PeerTransport {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(32 * 1024),
        }
    }

    pub async fn send_handshake(&mut self, handshake: &Handshake) -> Result<(), PeerError> {
        let data = handshake.encode();
        timeout(WRITE_TIMEOUT, self.stream.write_all(&data))
            .await
            .map_err(|_| PeerError::Timeout)??;
        Ok(())
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
        timeout(WRITE_TIMEOUT, self.stream.write_all(&data))
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

    pub fn into_inner(self) -> TcpStream {
        self.stream
    }

    pub fn peer_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.peer_addr()
    }
}
