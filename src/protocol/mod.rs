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
