//! UPnP and NAT-PMP port mapping.
//!
//! This module provides automatic port forwarding through UPnP IGD (Internet Gateway Device)
//! and NAT-PMP protocols. This allows BitTorrent clients behind NAT to receive incoming
//! connections.
//!
//! # Example
//!
//! ```no_run
//! use rbit::{PortMapper, PortMapping, Protocol};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut mapper = PortMapper::new();
//!
//! // Discover available gateways
//! mapper.discover().await?;
//!
//! // Map port 6881 TCP
//! let mapping = PortMapping {
//!     internal_port: 6881,
//!     external_port: 6881,
//!     protocol: Protocol::Tcp,
//!     lifetime: 3600,
//! };
//!
//! let external_port = mapper.add_mapping(&mapping).await?;
//! println!("Mapped to external port: {}", external_port);
//!
//! // Get external IP
//! let external_ip = mapper.get_external_ip().await?;
//! println!("External IP: {}", external_ip);
//! # Ok(())
//! # }
//! ```

use std::net::{Ipv4Addr, SocketAddrV4};

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::constants::{
    NATPMP_PORT, SSDP_MULTICAST, SSDP_PORT, UPNP_DISCOVERY_TIMEOUT, UPNP_REQUEST_TIMEOUT,
    UPNP_SOCKET_READ_TIMEOUT,
};

/// Errors that can occur during port mapping operations.
#[derive(Debug, Error)]
pub enum UpnpError {
    /// I/O error
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP error
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// UPnP protocol error
    #[error("upnp error: {0}")]
    Upnp(String),

    /// Operation timed out
    #[error("timeout")]
    Timeout,

    /// Invalid response from gateway
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// No mapping service available
    #[error("no mapping available")]
    NoMappingAvailable,
}

fn ssdp_multicast() -> Ipv4Addr {
    SSDP_MULTICAST.parse().expect("invalid SSDP_MULTICAST")
}

/// A port mapping configuration.
#[derive(Debug, Clone)]
pub struct PortMapping {
    /// The internal (local) port to map
    pub internal_port: u16,
    /// The requested external port (may differ from actual)
    pub external_port: u16,
    /// The protocol (TCP or UDP)
    pub protocol: Protocol,
    /// Lifetime in seconds (0 for indefinite)
    pub lifetime: u32,
}

/// Network protocol for port mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// TCP protocol
    Tcp,
    /// UDP protocol
    Udp,
}

impl Protocol {
    fn as_str(&self) -> &'static str {
        match self {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        }
    }

    fn natpmp_opcode(&self) -> u8 {
        match self {
            Protocol::Udp => 1,
            Protocol::Tcp => 2,
        }
    }
}

/// Port mapper supporting UPnP IGD and NAT-PMP.
///
/// Automatically discovers and uses available port mapping protocols.
pub struct PortMapper {
    gateway: Option<String>,
    control_url: Option<String>,
    natpmp_gateway: Option<Ipv4Addr>,
}

impl PortMapper {
    /// Creates a new port mapper.
    pub fn new() -> Self {
        Self {
            gateway: None,
            control_url: None,
            natpmp_gateway: None,
        }
    }

    /// Discovers available port mapping services.
    ///
    /// Tries UPnP first, then falls back to NAT-PMP.
    pub async fn discover(&mut self) -> Result<(), UpnpError> {
        if let Ok((gateway, control_url)) = self.discover_upnp().await {
            self.gateway = Some(gateway);
            self.control_url = Some(control_url);
            return Ok(());
        }

        if let Ok(gateway) = self.discover_natpmp().await {
            self.natpmp_gateway = Some(gateway);
            return Ok(());
        }

        Err(UpnpError::NoMappingAvailable)
    }

    async fn discover_upnp(&self) -> Result<(String, String), UpnpError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.set_broadcast(true)?;

        let search_request = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: {}:{}\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 3\r\n\
             ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\
             \r\n",
            ssdp_multicast(),
            SSDP_PORT
        );

        let dest = SocketAddrV4::new(ssdp_multicast(), SSDP_PORT);
        socket.send_to(search_request.as_bytes(), dest).await?;

        let mut buf = vec![0u8; 2048];
        let (n, _) = timeout(UPNP_DISCOVERY_TIMEOUT, socket.recv_from(&mut buf))
            .await
            .map_err(|_| UpnpError::Timeout)??;

        let response = std::str::from_utf8(&buf[..n])
            .map_err(|_| UpnpError::InvalidResponse("invalid utf8".into()))?;

        let location = response
            .lines()
            .find(|l| l.to_lowercase().starts_with("location:"))
            .map(|l| {
                let parts: Vec<&str> = l.splitn(2, ':').collect();
                if parts.len() > 1 {
                    parts[1].trim().to_string()
                } else {
                    String::new()
                }
            })
            .filter(|s| !s.is_empty())
            .ok_or_else(|| UpnpError::InvalidResponse("no location header".into()))?;

        let control_url = self.get_control_url(&location).await?;

        Ok((location, control_url))
    }

    async fn get_control_url(&self, location: &str) -> Result<String, UpnpError> {
        let client = reqwest::Client::new();
        let response = timeout(UPNP_REQUEST_TIMEOUT, client.get(location).send())
            .await
            .map_err(|_| UpnpError::Timeout)??;

        let body = response.text().await?;

        let service_type = "urn:schemas-upnp-org:service:WANIPConnection:1";
        if let Some(pos) = body.find(service_type) {
            let rest = &body[pos..];
            if let Some(url_start) = rest.find("<controlURL>") {
                let url_start = url_start + "<controlURL>".len();
                if let Some(url_end) = rest[url_start..].find("</controlURL>") {
                    let control_path = &rest[url_start..url_start + url_end];

                    if control_path.starts_with("http") {
                        return Ok(control_path.to_string());
                    } else {
                        let base = location
                            .rfind('/')
                            .map(|i| &location[..i])
                            .unwrap_or(location);
                        return Ok(format!("{}{}", base, control_path));
                    }
                }
            }
        }

        Err(UpnpError::InvalidResponse("control URL not found".into()))
    }

    async fn discover_natpmp(&self) -> Result<Ipv4Addr, UpnpError> {
        let gateways = [
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(192, 168, 0, 1),
            Ipv4Addr::new(10, 0, 0, 1),
        ];

        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        for gateway in gateways {
            let request = [0u8, 0];
            let dest = SocketAddrV4::new(gateway, NATPMP_PORT);

            if socket.send_to(&request, dest).await.is_ok() {
                let mut buf = vec![0u8; 16];
                if let Ok(Ok((n, _))) =
                    timeout(UPNP_SOCKET_READ_TIMEOUT, socket.recv_from(&mut buf)).await
                {
                    if n >= 12 && buf[0] == 0 && buf[1] == 128 && buf[3] == 0 {
                        return Ok(gateway);
                    }
                }
            }
        }

        Err(UpnpError::NoMappingAvailable)
    }

    /// Adds a port mapping.
    ///
    /// Returns the actual external port assigned (may differ from requested).
    pub async fn add_mapping(&self, mapping: &PortMapping) -> Result<u16, UpnpError> {
        if let Some(ref control_url) = self.control_url {
            return self.add_upnp_mapping(control_url, mapping).await;
        }

        if let Some(gateway) = self.natpmp_gateway {
            return self.add_natpmp_mapping(gateway, mapping).await;
        }

        Err(UpnpError::NoMappingAvailable)
    }

    async fn add_upnp_mapping(
        &self,
        control_url: &str,
        mapping: &PortMapping,
    ) -> Result<u16, UpnpError> {
        let local_ip = self
            .get_local_ip()
            .unwrap_or_else(|| Ipv4Addr::new(0, 0, 0, 0));

        let body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:AddPortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{}</NewExternalPort>
<NewProtocol>{}</NewProtocol>
<NewInternalPort>{}</NewInternalPort>
<NewInternalClient>{}</NewInternalClient>
<NewEnabled>1</NewEnabled>
<NewPortMappingDescription>rbit</NewPortMappingDescription>
<NewLeaseDuration>{}</NewLeaseDuration>
</u:AddPortMapping>
</s:Body>
</s:Envelope>"#,
            mapping.external_port,
            mapping.protocol.as_str(),
            mapping.internal_port,
            local_ip,
            mapping.lifetime
        );

        let client = reqwest::Client::new();
        let response = timeout(
            UPNP_REQUEST_TIMEOUT,
            client
                .post(control_url)
                .header("Content-Type", "text/xml")
                .header(
                    "SOAPAction",
                    "\"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\"",
                )
                .body(body)
                .send(),
        )
        .await
        .map_err(|_| UpnpError::Timeout)??;

        if response.status().is_success() {
            Ok(mapping.external_port)
        } else {
            Err(UpnpError::Upnp(format!(
                "mapping failed: {}",
                response.status()
            )))
        }
    }

    async fn add_natpmp_mapping(
        &self,
        gateway: Ipv4Addr,
        mapping: &PortMapping,
    ) -> Result<u16, UpnpError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        let mut request = Vec::with_capacity(12);
        request.push(0);
        request.push(mapping.protocol.natpmp_opcode());
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(&mapping.internal_port.to_be_bytes());
        request.extend_from_slice(&mapping.external_port.to_be_bytes());
        request.extend_from_slice(&mapping.lifetime.to_be_bytes());

        let dest = SocketAddrV4::new(gateway, NATPMP_PORT);
        socket.send_to(&request, dest).await?;

        let mut buf = vec![0u8; 16];
        let (n, _) = timeout(UPNP_REQUEST_TIMEOUT, socket.recv_from(&mut buf))
            .await
            .map_err(|_| UpnpError::Timeout)??;

        if n >= 16 && buf[3] == 0 {
            let external_port = u16::from_be_bytes([buf[10], buf[11]]);
            Ok(external_port)
        } else {
            Err(UpnpError::Upnp(format!("NAT-PMP error code: {}", buf[3])))
        }
    }

    /// Removes a port mapping.
    pub async fn remove_mapping(
        &self,
        external_port: u16,
        protocol: Protocol,
    ) -> Result<(), UpnpError> {
        if let Some(ref control_url) = self.control_url {
            return self
                .remove_upnp_mapping(control_url, external_port, protocol)
                .await;
        }

        if let Some(gateway) = self.natpmp_gateway {
            return self
                .remove_natpmp_mapping(gateway, external_port, protocol)
                .await;
        }

        Err(UpnpError::NoMappingAvailable)
    }

    async fn remove_upnp_mapping(
        &self,
        control_url: &str,
        external_port: u16,
        protocol: Protocol,
    ) -> Result<(), UpnpError> {
        let body = format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:DeletePortMapping xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewRemoteHost></NewRemoteHost>
<NewExternalPort>{}</NewExternalPort>
<NewProtocol>{}</NewProtocol>
</u:DeletePortMapping>
</s:Body>
</s:Envelope>"#,
            external_port,
            protocol.as_str()
        );

        let client = reqwest::Client::new();
        let _ = timeout(
            UPNP_REQUEST_TIMEOUT,
            client
                .post(control_url)
                .header("Content-Type", "text/xml")
                .header(
                    "SOAPAction",
                    "\"urn:schemas-upnp-org:service:WANIPConnection:1#DeletePortMapping\"",
                )
                .body(body)
                .send(),
        )
        .await;

        Ok(())
    }

    async fn remove_natpmp_mapping(
        &self,
        gateway: Ipv4Addr,
        external_port: u16,
        protocol: Protocol,
    ) -> Result<(), UpnpError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        let mut request = Vec::with_capacity(12);
        request.push(0);
        request.push(protocol.natpmp_opcode());
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(&external_port.to_be_bytes());
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(&0u32.to_be_bytes());

        let dest = SocketAddrV4::new(gateway, NATPMP_PORT);
        socket.send_to(&request, dest).await?;

        Ok(())
    }

    /// Gets the external IP address from the gateway.
    pub async fn get_external_ip(&self) -> Result<Ipv4Addr, UpnpError> {
        if let Some(ref control_url) = self.control_url {
            return self.get_upnp_external_ip(control_url).await;
        }

        if let Some(gateway) = self.natpmp_gateway {
            return self.get_natpmp_external_ip(gateway).await;
        }

        Err(UpnpError::NoMappingAvailable)
    }

    async fn get_upnp_external_ip(&self, control_url: &str) -> Result<Ipv4Addr, UpnpError> {
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:GetExternalIPAddress xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
</u:GetExternalIPAddress>
</s:Body>
</s:Envelope>"#;

        let client = reqwest::Client::new();
        let response = timeout(
            UPNP_REQUEST_TIMEOUT,
            client
                .post(control_url)
                .header("Content-Type", "text/xml")
                .header(
                    "SOAPAction",
                    "\"urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress\"",
                )
                .body(body)
                .send(),
        )
        .await
        .map_err(|_| UpnpError::Timeout)??;

        let body = response.text().await?;

        if let Some(start) = body.find("<NewExternalIPAddress>") {
            let start = start + "<NewExternalIPAddress>".len();
            if let Some(end) = body[start..].find("</NewExternalIPAddress>") {
                let ip_str = &body[start..start + end];
                if let Ok(ip) = ip_str.parse() {
                    return Ok(ip);
                }
            }
        }

        Err(UpnpError::InvalidResponse("external IP not found".into()))
    }

    async fn get_natpmp_external_ip(&self, gateway: Ipv4Addr) -> Result<Ipv4Addr, UpnpError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;

        let request = [0u8, 0];
        let dest = SocketAddrV4::new(gateway, NATPMP_PORT);
        socket.send_to(&request, dest).await?;

        let mut buf = vec![0u8; 16];
        let (n, _) = timeout(UPNP_REQUEST_TIMEOUT, socket.recv_from(&mut buf))
            .await
            .map_err(|_| UpnpError::Timeout)??;

        if n >= 12 && buf[3] == 0 {
            let ip = Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]);
            Ok(ip)
        } else {
            Err(UpnpError::Upnp(format!("NAT-PMP error: {}", buf[3])))
        }
    }

    /// Returns whether a mapping service is available.
    pub fn is_available(&self) -> bool {
        self.control_url.is_some() || self.natpmp_gateway.is_some()
    }

    fn get_local_ip(&self) -> Option<Ipv4Addr> {
        use std::net::UdpSocket;

        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("8.8.8.8:80").ok()?;
        let addr = socket.local_addr().ok()?;
        match addr.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(_) => None,
        }
    }
}

impl Default for PortMapper {
    fn default() -> Self {
        Self::new()
    }
}
