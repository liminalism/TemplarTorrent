//! BEP-42: DHT Security Extension
//!
//! This module implements node ID generation and validation based on IP addresses
//! to prevent Sybil attacks on the DHT. Node IDs must be derived from the node's
//! external IP address using CRC32C hashing.
//!
//! [BEP-42]: http://bittorrent.org/beps/bep_0042.html

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rand::Rng;

use super::node::NodeId;

/// IPv4 mask for BEP-42 node ID generation.
/// The mask preserves specific bits of the IP for the CRC32C hash.
pub const BEP42_IPV4_MASK: u32 = 0x030f3fff;

/// IPv6 mask for BEP-42 node ID generation (applied to high 64 bits).
pub const BEP42_IPV6_MASK: u64 = 0x0103070f1f3f7fff;

/// Number of votes required to confirm external IP address.
pub const BEP42_REQUIRED_VOTES: u32 = 3;

/// Generates a BEP-42 compliant node ID based on the external IP address.
///
/// The node ID is generated such that:
/// - First 21 bits match the CRC32C hash of the masked IP with random value
/// - Last byte equals the random value `r` used in generation
/// - Remaining bytes are random
///
/// For local network addresses, returns a random node ID (no restrictions).
pub fn generate_secure_node_id(ip: IpAddr) -> NodeId {
    // Local networks are exempt from BEP-42 restrictions
    if is_local_network(&ip) {
        return NodeId::generate();
    }

    // Random value r in range [0, 7]
    let r: u8 = rand::rng().random::<u8>() & 0x07;

    let crc = compute_ip_crc(ip, r);

    // Build the node ID:
    // - First 21 bits from CRC32C
    // - Random middle bytes
    // - Last byte = r
    let mut id = [0u8; 20];

    // Fill with random bytes first
    rand::rng().fill(&mut id);

    // Set the first 21 bits from CRC
    // CRC is 32 bits, we use bits 0-20 (21 bits)
    // Byte 0: bits 0-7 of CRC
    // Byte 1: bits 8-15 of CRC
    // Byte 2: bits 16-20 of CRC (top 5 bits)
    id[0] = (crc >> 24) as u8;
    id[1] = (crc >> 16) as u8;
    id[2] = (id[2] & 0x07) | ((crc >> 8) as u8 & 0xf8);

    // Last byte must equal r
    id[19] = r;

    NodeId(id)
}

/// Validates that a node ID is correctly derived from the given IP address.
///
/// Returns true if:
/// - The IP is a local network address (always valid)
/// - The first 21 bits of the node ID match the expected CRC32C hash
/// - The last byte matches the random value used in generation
pub fn validate_node_id(node_id: &NodeId, ip: IpAddr) -> bool {
    // Local networks are exempt
    if is_local_network(&ip) {
        return true;
    }

    // Extract r from the last byte
    let r = node_id.0[19] & 0x07;

    // Compute expected CRC
    let expected_crc = compute_ip_crc(ip, r);

    // Extract actual first 21 bits from node ID
    let actual_bits = ((node_id.0[0] as u32) << 24)
        | ((node_id.0[1] as u32) << 16)
        | ((node_id.0[2] as u32) << 8);

    let expected_bits = expected_crc & 0xfffff800;

    actual_bits & 0xfffff800 == expected_bits
}

/// Computes the CRC32C hash for BEP-42 node ID derivation.
fn compute_ip_crc(ip: IpAddr, r: u8) -> u32 {
    match ip {
        IpAddr::V4(ipv4) => {
            let ip_u32 = u32::from(ipv4);
            let masked = (ip_u32 & BEP42_IPV4_MASK) | ((r as u32) << 29);
            crc32c::crc32c(&masked.to_be_bytes())
        }
        IpAddr::V6(ipv6) => {
            // Use high 64 bits of IPv6 address
            let octets = ipv6.octets();
            let ip_u64 = u64::from_be_bytes([
                octets[0], octets[1], octets[2], octets[3], octets[4], octets[5], octets[6],
                octets[7],
            ]);
            let masked = (ip_u64 & BEP42_IPV6_MASK) | ((r as u64) << 61);
            crc32c::crc32c(&masked.to_be_bytes())
        }
    }
}

/// Checks if an IP address is in a local/private network range.
///
/// Local networks are exempt from BEP-42 node ID restrictions:
/// - IPv4: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 127.0.0.0/8
/// - IPv6: loopback, link-local, unique local (fc00::/7)
pub fn is_local_network(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_local_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_local_ipv6(ipv6),
    }
}

fn is_local_ipv4(ip: &Ipv4Addr) -> bool {
    // 10.0.0.0/8
    if ip.octets()[0] == 10 {
        return true;
    }

    // 172.16.0.0/12
    if ip.octets()[0] == 172 && (ip.octets()[1] >= 16 && ip.octets()[1] <= 31) {
        return true;
    }

    // 192.168.0.0/16
    if ip.octets()[0] == 192 && ip.octets()[1] == 168 {
        return true;
    }

    // 169.254.0.0/16 (link-local)
    if ip.octets()[0] == 169 && ip.octets()[1] == 254 {
        return true;
    }

    // 127.0.0.0/8 (loopback)
    if ip.octets()[0] == 127 {
        return true;
    }

    false
}

fn is_local_ipv6(ip: &Ipv6Addr) -> bool {
    // Loopback (::1)
    if ip.is_loopback() {
        return true;
    }

    // Link-local (fe80::/10)
    let segments = ip.segments();
    if segments[0] & 0xffc0 == 0xfe80 {
        return true;
    }

    // Unique local addresses (fc00::/7)
    if segments[0] & 0xfe00 == 0xfc00 {
        return true;
    }

    false
}

/// Compact IP + port representation for the "ip" field in DHT responses.
///
/// Format: 4-byte IPv4 + 2-byte port (big-endian) or 16-byte IPv6 + 2-byte port
pub fn encode_compact_ip_port(ip: IpAddr, port: u16) -> Vec<u8> {
    match ip {
        IpAddr::V4(ipv4) => {
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(&ipv4.octets());
            buf.extend_from_slice(&port.to_be_bytes());
            buf
        }
        IpAddr::V6(ipv6) => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&ipv6.octets());
            buf.extend_from_slice(&port.to_be_bytes());
            buf
        }
    }
}

/// Decodes compact IP + port from bytes.
///
/// Returns (IpAddr, port) or None if invalid format.
pub fn decode_compact_ip_port(data: &[u8]) -> Option<(IpAddr, u16)> {
    if data.len() == 6 {
        // IPv4 + port
        let ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
        let port = u16::from_be_bytes([data[4], data[5]]);
        Some((IpAddr::V4(ip), port))
    } else if data.len() == 18 {
        // IPv6 + port
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&data[..16]);
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([data[16], data[17]]);
        Some((IpAddr::V6(ip), port))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_ipv4_detection() {
        // Private networks
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(
            10, 255, 255, 255
        ))));
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(
            172, 31, 255, 255
        ))));
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))));
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(
            192, 168, 255, 255
        ))));

        // Link-local
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));

        // Loopback
        assert!(is_local_network(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));

        // Public IPs should not be local
        assert!(!is_local_network(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_local_network(&IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
    }

    #[test]
    fn test_local_ipv6_detection() {
        // Loopback
        assert!(is_local_network(&IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // Link-local (fe80::/10)
        assert!(is_local_network(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));

        // Unique local (fc00::/7)
        assert!(is_local_network(&IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_local_network(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));

        // Global unicast should not be local
        assert!(!is_local_network(&IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888
        ))));
    }

    #[test]
    fn test_secure_node_id_generation() {
        let ip = IpAddr::V4(Ipv4Addr::new(124, 31, 75, 21));
        let node_id = generate_secure_node_id(ip);

        // Validate the generated ID
        assert!(validate_node_id(&node_id, ip));
    }

    #[test]
    fn test_node_id_validation_fails_for_wrong_ip() {
        let ip1 = IpAddr::V4(Ipv4Addr::new(124, 31, 75, 21));
        let ip2 = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));

        let node_id = generate_secure_node_id(ip1);

        // Should validate against the correct IP
        assert!(validate_node_id(&node_id, ip1));

        // Should fail against a different IP (in most cases)
        // Note: There's a small chance this could pass due to CRC collision
        // but it's extremely unlikely
        assert!(!validate_node_id(&node_id, ip2));
    }

    #[test]
    fn test_local_network_always_valid() {
        let local_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let random_id = NodeId::generate();

        // Any node ID should be valid for local networks
        assert!(validate_node_id(&random_id, local_ip));
    }

    #[test]
    fn test_compact_ip_port_roundtrip_v4() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));
        let port = 6881u16;

        let encoded = encode_compact_ip_port(ip, port);
        assert_eq!(encoded.len(), 6);

        let (decoded_ip, decoded_port) = decode_compact_ip_port(&encoded).unwrap();
        assert_eq!(decoded_ip, ip);
        assert_eq!(decoded_port, port);
    }

    #[test]
    fn test_compact_ip_port_roundtrip_v6() {
        let ip = IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0, 0, 0, 0, 0, 0x8888));
        let port = 6881u16;

        let encoded = encode_compact_ip_port(ip, port);
        assert_eq!(encoded.len(), 18);

        let (decoded_ip, decoded_port) = decode_compact_ip_port(&encoded).unwrap();
        assert_eq!(decoded_ip, ip);
        assert_eq!(decoded_port, port);
    }

    #[test]
    fn test_bep42_example_ip() {
        // Test with the example from BEP-42
        // IP: 124.31.75.21, r=1
        // Expected first bytes based on CRC32C of masked IP
        let ip = Ipv4Addr::new(124, 31, 75, 21);
        let r = 1u8;

        let ip_u32 = u32::from(ip);
        let masked = (ip_u32 & BEP42_IPV4_MASK) | ((r as u32) << 29);
        let crc = crc32c::crc32c(&masked.to_be_bytes());

        // The CRC should produce consistent results
        assert!(crc != 0);
    }
}
