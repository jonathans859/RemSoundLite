//! The RemSound UDP wire format, header version 1, as RemSound and remremote-audio speak it.
//!
//! Every datagram starts with a 12-byte little-endian header:
//! `u32 magic "RMND" | u8 version (1) | u8 type | u16 stream id | u32 sequence`.
//! The body length is whatever is left of the datagram. Stream id 0 is read as 1.

pub const HEADER_SIZE: usize = 12;
const MAGIC: u32 = 0x444E_4D52; // "RMND" read as little-endian
const VERSION: u8 = 1;

/// The single port RemSound uses for audio, heartbeats and everything that goes with them.
pub const AUDIO_PORT: u16 = 47830;
/// Heartbeats and tick proofs travel on this reserved stream id.
pub const CONTROL_STREAM_ID: u16 = 0xFFFF;
/// remremote's sealed hello/pong packets use this one; we never open an audio stream on it.
pub const HELLO_STREAM_ID: u16 = 0xFFFE;

/// The largest audio payload that fits a 1500-byte path without fragmentation
/// (1500 - 20 IP - 8 UDP - 12 header - 6 PCM sub-header).
pub const MAX_AUDIO_PAYLOAD: usize = 1454;

pub const CODEC_PCM: i32 = 1;
pub const CODEC_OPUS: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    Format,
    Audio,
    KeepAlive,
    Heartbeat,
    Control,
    AddrCheck,
    TickProof,
    Other(u8),
}

impl PacketType {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Format,
            2 => Self::Audio,
            3 => Self::KeepAlive,
            4 => Self::Heartbeat,
            5 => Self::Control,
            10 => Self::AddrCheck,
            11 => Self::TickProof,
            other => Self::Other(other),
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Format => 1,
            Self::Audio => 2,
            Self::KeepAlive => 3,
            Self::Heartbeat => 4,
            Self::Control => 5,
            Self::AddrCheck => 10,
            Self::TickProof => 11,
            Self::Other(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub packet_type: PacketType,
    pub stream_id: u16,
    pub sequence: u32,
}

impl Header {
    pub fn new(packet_type: PacketType, stream_id: u16, sequence: u32) -> Self {
        Self { packet_type, stream_id, sequence }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.push(VERSION);
        out.push(self.packet_type.to_u8());
        let stream_id = if self.stream_id == 0 { 1 } else { self.stream_id };
        out.extend_from_slice(&stream_id.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
    }

    /// `None` for anything that is not a RemSound version 1 datagram.
    pub fn parse(packet: &[u8]) -> Option<Self> {
        if packet.len() < HEADER_SIZE {
            return None;
        }
        if u32::from_le_bytes(packet[0..4].try_into().ok()?) != MAGIC || packet[4] != VERSION {
            return None;
        }
        let stream_id = match u16::from_le_bytes([packet[6], packet[7]]) {
            0 => 1,
            id => id,
        };
        Some(Self {
            packet_type: PacketType::from_u8(packet[5]),
            stream_id,
            sequence: u32::from_le_bytes(packet[8..12].try_into().ok()?),
        })
    }
}

/// The Format payload: what a sender's stream looks like. 32 bytes from old senders, 36 with
/// the lane byte, 44 with the password fingerprint and 46 with the sender's capture latency.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioFormat {
    pub sample_rate: i32,
    pub channels: i32,
    pub bits_per_sample: i32,
    pub encoding: i32,
    pub block_align: i32,
    pub average_bytes_per_second: i32,
    pub codec: i32,
    /// Samples per channel in one frame, at `sample_rate`.
    pub frame_samples: i32,
    pub fingerprint: Option<[u8; 8]>,
    /// The sender's own capture latency in milliseconds; 0 when it didn't say.
    pub capture_latency_ms: f32,
}

impl AudioFormat {
    /// What RemSound announces for an Opus stream.
    pub fn opus(frame_samples: i32, fingerprint: [u8; 8], capture_latency_ms: f32) -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
            encoding: 1,
            block_align: 4,
            average_bytes_per_second: 192_000,
            codec: CODEC_OPUS,
            frame_samples,
            fingerprint: Some(fingerprint),
            capture_latency_ms,
        }
    }

    /// What RemSound announces for 24-bit PCM.
    pub fn pcm24(frame_samples: i32, fingerprint: [u8; 8], capture_latency_ms: f32) -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 24,
            encoding: 1,
            block_align: 6,
            average_bytes_per_second: 288_000,
            codec: CODEC_PCM,
            frame_samples,
            fingerprint: Some(fingerprint),
            capture_latency_ms,
        }
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        for value in [
            self.sample_rate,
            self.channels,
            self.bits_per_sample,
            self.encoding,
            self.block_align,
            self.average_bytes_per_second,
            self.codec,
            self.frame_samples,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0, 0, 0]); // lane (mixed), flags, two reserved bytes
        if let Some(fingerprint) = &self.fingerprint {
            out.extend_from_slice(fingerprint);
            let tenths = (self.capture_latency_ms * 10.0).round().clamp(0.0, u16::MAX as f32) as u16;
            out.extend_from_slice(&tenths.to_le_bytes());
        }
    }

    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 32 {
            return None;
        }
        let int = |at: usize| i32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
        let fingerprint = (payload.len() >= 44).then(|| payload[36..44].try_into().unwrap());
        let capture_latency_ms = if payload.len() >= 46 {
            u16::from_le_bytes([payload[44], payload[45]]) as f32 / 10.0
        } else {
            0.0
        };
        Some(Self {
            sample_rate: int(0),
            channels: int(4),
            bits_per_sample: int(8),
            encoding: int(12),
            block_align: int(16),
            average_bytes_per_second: int(20),
            codec: int(24),
            frame_samples: int(28),
            fingerprint,
            capture_latency_ms,
        })
    }

    pub fn same_stream(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.sample_rate == other.sample_rate
            && self.channels == other.channels
            && self.bits_per_sample == other.bits_per_sample
            && self.frame_samples == other.frame_samples
    }
}

pub const HEARTBEAT_PING: u8 = 0;
pub const HEARTBEAT_PONG: u8 = 1;

/// Heartbeat payload: kind (ping/pong) and the pinger's own monotonic milliseconds, which the
/// pong echoes back so the round trip is measured against one clock.
pub fn write_heartbeat(out: &mut Vec<u8>, kind: u8, originator_ms: i64) {
    out.push(kind);
    out.extend_from_slice(&originator_ms.to_le_bytes());
}

pub fn parse_heartbeat(payload: &[u8]) -> Option<(u8, i64)> {
    if payload.len() < 9 || payload[0] > HEARTBEAT_PONG {
        return None;
    }
    Some((payload[0], i64::from_le_bytes(payload[1..9].try_into().ok()?)))
}

/// PCM frames are sealed whole and the ciphertext is cut into parts, each carrying
/// `u32 frame id | u8 part index | u8 part count` in front of its share of the ciphertext.
pub const PCM_SUBHEADER_SIZE: usize = 6;

pub fn write_pcm_subheader(out: &mut Vec<u8>, frame_id: u32, part: u8, parts: u8) {
    out.extend_from_slice(&frame_id.to_le_bytes());
    out.push(part);
    out.push(parts);
}

pub fn parse_pcm_subheader(payload: &[u8]) -> Option<(u32, u8, u8)> {
    if payload.len() < PCM_SUBHEADER_SIZE {
        return None;
    }
    let (part, parts) = (payload[4], payload[5]);
    (parts > 0 && part < parts).then(|| (u32::from_le_bytes(payload[0..4].try_into().unwrap()), part, parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let mut bytes = Vec::new();
        Header::new(PacketType::Heartbeat, CONTROL_STREAM_ID, 42).write(&mut bytes);
        assert_eq!(&bytes[..4], b"RMND");
        let header = Header::parse(&bytes).unwrap();
        assert_eq!(header.packet_type, PacketType::Heartbeat);
        assert_eq!(header.stream_id, CONTROL_STREAM_ID);
        assert_eq!(header.sequence, 42);
        let mut zero = Vec::new();
        Header::new(PacketType::Audio, 0, 1).write(&mut zero);
        assert_eq!(Header::parse(&zero).unwrap().stream_id, 1);
        bytes[4] = 2;
        assert!(Header::parse(&bytes).is_none());
    }

    #[test]
    fn format_round_trip_and_short_payloads() {
        let format = AudioFormat::opus(120, [1, 2, 3, 4, 5, 6, 7, 8], 10.5);
        let mut payload = Vec::new();
        format.write(&mut payload);
        assert_eq!(payload.len(), 46);
        let parsed = AudioFormat::parse(&payload).unwrap();
        assert_eq!(parsed, format);
        let legacy = AudioFormat::parse(&payload[..32]).unwrap();
        assert_eq!(legacy.fingerprint, None);
        assert!(legacy.same_stream(&format));
        assert!(AudioFormat::parse(&payload[..31]).is_none());
    }

    #[test]
    fn heartbeat_and_pcm_subheader() {
        let mut payload = Vec::new();
        write_heartbeat(&mut payload, HEARTBEAT_PONG, -7);
        assert_eq!(parse_heartbeat(&payload), Some((HEARTBEAT_PONG, -7)));
        let mut sub = Vec::new();
        write_pcm_subheader(&mut sub, 9, 1, 2);
        assert_eq!(parse_pcm_subheader(&sub), Some((9, 1, 2)));
        sub[4] = 2;
        assert_eq!(parse_pcm_subheader(&sub), None);
    }
}
