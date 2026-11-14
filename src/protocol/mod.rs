//! The RemSound wire protocol: packets, encryption and discovery.

pub mod crypto;
pub mod discovery;
pub mod packet;

use packet::{CONTROL_STREAM_ID, Header, PacketType};
use uuid::Uuid;

/// A tick proof: "I have ticked you and hold the password", sent every few seconds to each
/// ticked peer so a RemSound that only listens can still accept us. The sealed plaintext is
/// `version (1) | unix seconds (i64 LE) | our discovery instance id (16 bytes, RFC 4122 order)`.
pub fn tick_proof(sealer: &mut crypto::Sealer, instance_id: Uuid, sequence: u32) -> Vec<u8> {
    let unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut plain = Vec::with_capacity(25);
    plain.push(1);
    plain.extend_from_slice(&unix_seconds.to_le_bytes());
    plain.extend_from_slice(instance_id.as_bytes());
    let mut packet = Vec::with_capacity(packet::HEADER_SIZE + 25 + crypto::OVERHEAD);
    Header::new(PacketType::TickProof, CONTROL_STREAM_ID, sequence).write(&mut packet);
    sealer.seal(&plain, &mut packet);
    packet
}

/// Why a peer's address couldn't be turned into an IPv4 address.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The name doesn't exist, or the lookup failed or took too long.
    NotFound,
    /// The name exists but has only IPv6 addresses, which RemSound doesn't use.
    NoIpv4,
}

/// The IPv4 address of a peer typed as an address or a host name (a domain, or a Tailscale
/// MagicDNS name). An address is used as is; a name is looked up once, waiting at most
/// `timeout`, since a failed lookup on Windows can take several seconds.
pub fn resolve_peer(text: &str, timeout: std::time::Duration) -> Result<std::net::Ipv4Addr, ResolveError> {
    use std::net::ToSocketAddrs;
    if let Ok(address) = text.parse() {
        return Ok(address);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let name = text.to_string();
    // A lookup that outlives the timeout is abandoned; the thread ends when it does.
    std::thread::spawn(move || {
        let found: Vec<_> = (name.as_str(), packet::AUDIO_PORT).to_socket_addrs().map(Iterator::collect).unwrap_or_default();
        let _ = tx.send(found);
    });
    let found = rx.recv_timeout(timeout).unwrap_or_default();
    first_ipv4(&found)
}

/// The first IPv4 address of a lookup's results.
fn first_ipv4(found: &[std::net::SocketAddr]) -> Result<std::net::Ipv4Addr, ResolveError> {
    if found.is_empty() {
        return Err(ResolveError::NotFound);
    }
    found
        .iter()
        .find_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
            std::net::SocketAddr::V6(_) => None,
        })
        .ok_or(ResolveError::NoIpv4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::Duration;

    #[test]
    fn addresses_need_no_lookup() {
        assert_eq!(resolve_peer("100.110.55.121", Duration::ZERO), Ok(Ipv4Addr::new(100, 110, 55, 121)));
    }

    #[test]
    fn localhost_resolves() {
        assert_eq!(resolve_peer("localhost", Duration::from_secs(5)), Ok(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn picks_the_first_ipv4_after_ipv6() {
        let found: Vec<SocketAddr> =
            vec!["[fd7a:115c:a1e0::1]:1".parse().unwrap(), "100.64.0.2:1".parse().unwrap(), "100.64.0.3:1".parse().unwrap()];
        assert_eq!(first_ipv4(&found), Ok(Ipv4Addr::new(100, 64, 0, 2)));
        assert_eq!(first_ipv4(&found[..1]), Err(ResolveError::NoIpv4));
        assert_eq!(first_ipv4(&[]), Err(ResolveError::NotFound));
    }
}
