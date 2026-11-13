//! The streaming engine: everything that happens on the network and in the sound devices.
//!
//! Like RemSound, everything travels on one UDP port (47830): audio both ways, Format
//! announcements, heartbeats (ping/pong once a second per ticked peer, for the link state
//! and round-trip time) and tick proofs. Discovery has its own port (47821).
//!
//! * Sending: every ticked source is captured into its own queue; a sender thread running on
//!   the system clock mixes them into one 48 kHz stereo stream, cuts it into frames (Opus or
//!   24-bit PCM), seals each with AES-256-GCM and sends it to every ticked peer.
//! * Receiving: audio is accepted only from ticked peers whose password matches; each stream
//!   is decoded into a playout buffer on every ticked output device.
//!
//! The GUI (or the service) owns one `Engine`, calls `apply` whenever a setting changes and
//! reads `snapshot` to show what is going on.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::audio::capture::Capture;
use crate::audio::jitter::{Artefact, BufferParams};
use crate::audio::opus;
use crate::audio::render::{Output, StreamKey};
use crate::profile::{self, Codec, PacketSize, Profile};
use crate::protocol::crypto::{Keys, OVERHEAD, Opener, Sealer};
use crate::protocol::discovery::{Announcement, DiscoveredPeer, Discovery};
use crate::protocol::packet::{
    self, AUDIO_PORT, AudioFormat, CODEC_OPUS, CODEC_PCM, CONTROL_STREAM_ID, HEADER_SIZE, HELLO_STREAM_ID, Header,
    MAX_AUDIO_PAYLOAD, PacketType,
};

const OPUS_BITRATE: i32 = 192_000;
const FORMAT_INTERVAL: Duration = Duration::from_millis(250);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const TICK_PROOF_INTERVAL: Duration = Duration::from_secs(5);
const AUTO_TUNE_INTERVAL: Duration = Duration::from_secs(5);
const HEALTHY_WINDOW: Duration = Duration::from_secs(2);
const UNREACHABLE_WINDOW: Duration = Duration::from_secs(5);
/// A peer asking to connect is listed this long after its last tick proof (sent every 5 s).
const INCOMING_WINDOW: Duration = Duration::from_secs(15);
/// Tick proofs older or newer than this (peers' clocks aren't synchronised) are refused.
const TICK_PROOF_SKEW_SECS: i64 = 600;

#[derive(Clone, Debug, PartialEq)]
pub struct PeerTarget {
    pub address: Ipv4Addr,
    pub name: String,
}

/// Everything the engine needs to know, derived from a profile.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineConfig {
    pub name: String,
    pub password: String,
    /// The ticked peers.
    pub peers: Vec<PeerTarget>,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub sources: Vec<String>,
    pub outputs: Vec<String>,
    pub codec: Codec,
    pub packet_size: PacketSize,
    pub jitter_ms: u32,
    pub auto_tune: bool,
    pub smoothness: u8,
    pub artefact: Artefact,
    /// Volume of received audio, 0-100 percent.
    pub volume: u8,
}

pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_string())
}

impl EngineConfig {
    /// `send_only` is the lock-screen service, which never plays anything.
    pub fn from_profile(profile: &Profile, send_only: bool) -> Self {
        let peers = profile
            .peers
            .iter()
            .filter(|p| p.active)
            .filter_map(|p| Some(PeerTarget { address: p.address.trim().parse().ok()?, name: p.name.clone() }))
            .collect();
        Self {
            name: machine_name(),
            password: profile.password.clone(),
            peers,
            send_enabled: profile.send_enabled,
            receive_enabled: profile.receive_enabled && !send_only,
            sources: profile.sources.iter().map(|d| d.id.clone()).collect(),
            outputs: profile.outputs.iter().map(|d| d.id.clone()).collect(),
            codec: profile.codec,
            packet_size: profile.packet_size,
            jitter_ms: profile.jitter_ms,
            auto_tune: profile.auto_tune,
            smoothness: profile.smoothness,
            artefact: profile.artefact,
            volume: profile.volume,
        }
    }

    fn is_peer(&self, address: Ipv4Addr) -> bool {
        self.peers.iter().any(|p| p.address == address)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    Connecting,
    Healthy,
    Stale,
    Unreachable,
}

#[derive(Clone, Debug)]
pub struct PeerSnapshot {
    pub address: Ipv4Addr,
    pub name: String,
    /// Since the first pong after the peer was ticked.
    pub connected_for: Option<Duration>,
    pub rtt_ms: Option<u32>,
    pub health: Health,
    /// What the peer last announced it sends us.
    pub their_format: Option<AudioFormat>,
    pub receiving_their_audio: bool,
    pub password_mismatch: bool,
    /// From discovery: whether the peer says it receives audio. `None` when not discovered.
    pub can_receive: Option<bool>,
}

/// A computer that isn't one of our ticked peers but has ticked us, and proved it holds the
/// password with a sealed tick proof.
#[derive(Clone, Debug, PartialEq)]
pub struct IncomingPeer {
    pub address: Ipv4Addr,
    pub instance_id: Uuid,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub uptime: Duration,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub port_error: Option<String>,
    pub discovery_error: Option<String>,
    pub keys_ready: bool,
    pub peers: Vec<PeerSnapshot>,
    pub discovered: Vec<DiscoveredPeer>,
    /// Peers asking to connect: they ticked us, we haven't ticked them.
    pub incoming: Vec<IncomingPeer>,
    pub sources_running: usize,
    pub source_errors: Vec<String>,
    pub output_errors: Vec<String>,
    /// The longest output device buffer, in ms.
    pub output_latency_ms: f32,
    /// The jitter buffer in use (the setting, or what auto-tune has made of it).
    pub jitter_ms: u32,
    /// Audio frames went out within the last second.
    pub sending: bool,
}

struct PeerState {
    added: Instant,
    first_pong: Option<Instant>,
    last_pong: Option<Instant>,
    rtt_ms: Option<u32>,
    their_format: Option<AudioFormat>,
    last_audio: Option<Instant>,
    password_mismatch: bool,
}

impl PeerState {
    fn new() -> Self {
        Self {
            added: Instant::now(),
            first_pong: None,
            last_pong: None,
            rtt_ms: None,
            their_format: None,
            last_audio: None,
            password_mismatch: false,
        }
    }
}

struct Shared {
    config: Mutex<EngineConfig>,
    config_generation: AtomicU64,
    keys: Mutex<Option<(String, Keys)>>,
    socket: Mutex<Option<Arc<UdpSocket>>>,
    port_error: Mutex<Option<String>>,
    peers: Mutex<HashMap<Ipv4Addr, PeerState>>,
    incoming: Mutex<HashMap<Ipv4Addr, (Uuid, Instant)>>,
    sources: Mutex<Vec<(String, Capture)>>,
    outputs: Mutex<Vec<(Option<String>, Output)>>,
    bytes_received: AtomicU64,
    bytes_sent: AtomicU64,
    /// Milliseconds since start at which audio last went out.
    last_send_ms: AtomicU64,
    tuned_jitter_ms: AtomicU32,
    stop: AtomicBool,
    instance_id: Uuid,
    started: Instant,
    discovery: Discovery,
}

impl Shared {
    fn now_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    fn socket(&self) -> Option<Arc<UdpSocket>> {
        self.socket.lock().unwrap().clone()
    }

    fn send_to(&self, socket: &UdpSocket, packet: &[u8], address: Ipv4Addr) {
        if socket.send_to(packet, SocketAddrV4::new(address, AUDIO_PORT)).is_ok() {
            self.bytes_sent.fetch_add(packet.len() as u64, Ordering::Relaxed);
        }
    }

    fn keys(&self) -> Option<Keys> {
        self.keys.lock().unwrap().as_ref().map(|(_, k)| k.clone())
    }

    fn buffer_params(&self, config: &EngineConfig) -> BufferParams {
        BufferParams {
            target_ms: self.tuned_jitter_ms.load(Ordering::Relaxed),
            smoothness: config.smoothness,
            artefact: config.artefact,
        }
    }
}

pub struct Engine {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

fn bind_audio_port() -> Result<UdpSocket, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    let _ = socket.set_recv_buffer_size(1 << 20);
    let _ = socket.set_send_buffer_size(1 << 18);
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, AUDIO_PORT));
    socket
        .bind(&address.into())
        .map_err(|e| format!("UDP port {AUDIO_PORT} is in use by another program ({e})"))?;
    let socket: UdpSocket = socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(250))).map_err(|e| e.to_string())?;
    Ok(socket)
}

impl Engine {
    pub fn start(config: EngineConfig) -> Self {
        let instance_id = Uuid::new_v4();
        let discovery = Discovery::start(instance_id, announcement(&config));
        let shared = Arc::new(Shared {
            tuned_jitter_ms: AtomicU32::new(config.jitter_ms),
            config: Mutex::new(config.clone()),
            config_generation: AtomicU64::new(0),
            keys: Mutex::new(None),
            socket: Mutex::new(None),
            port_error: Mutex::new(None),
            peers: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            sources: Mutex::new(Vec::new()),
            outputs: Mutex::new(Vec::new()),
            bytes_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            last_send_ms: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            instance_id,
            started: Instant::now(),
            discovery,
        });
        let mut engine = Self { shared: shared.clone(), threads: Vec::new() };
        let s = shared.clone();
        engine.threads.push(thread::Builder::new().name("net".into()).spawn(move || net_loop(&s)).unwrap());
        let s = shared.clone();
        engine.threads.push(thread::Builder::new().name("timers".into()).spawn(move || timer_loop(&s)).unwrap());
        let s = shared.clone();
        engine.threads.push(thread::Builder::new().name("sender".into()).spawn(move || sender_loop(&s)).unwrap());
        engine.reconcile(None, &config);
        engine
    }

    /// Applies new settings: starts and stops devices, re-derives the key when the password
    /// changed, and adopts the new peer list.
    pub fn apply(&self, config: EngineConfig) {
        let old = {
            let mut current = self.shared.config.lock().unwrap();
            if *current == config {
                return;
            }
            std::mem::replace(&mut *current, config.clone())
        };
        self.shared.config_generation.fetch_add(1, Ordering::Relaxed);
        if old.jitter_ms != config.jitter_ms || old.auto_tune != config.auto_tune {
            self.shared.tuned_jitter_ms.store(config.jitter_ms, Ordering::Relaxed);
        }
        self.reconcile(Some(&old), &config);
    }

    fn reconcile(&self, old: Option<&EngineConfig>, config: &EngineConfig) {
        let shared = &self.shared;
        if old.is_none_or(|o| o.password != config.password) {
            let password = config.password.clone();
            if password.is_empty() {
                *shared.keys.lock().unwrap() = None;
            } else {
                let s = shared.clone();
                thread::spawn(move || {
                    let keys = Keys::derive(&password);
                    if s.config.lock().unwrap().password == password {
                        *s.keys.lock().unwrap() = Some((password, keys));
                    }
                });
            }
        }

        {
            let mut peers = shared.peers.lock().unwrap();
            peers.retain(|address, _| config.is_peer(*address));
            for peer in &config.peers {
                peers.entry(peer.address).or_insert_with(PeerState::new);
            }
        }

        let wanted_sources: Vec<String> = if config.send_enabled { config.sources.clone() } else { Vec::new() };
        {
            let mut sources = shared.sources.lock().unwrap();
            let removed: Vec<(String, Capture)> = {
                let (keep, drop): (Vec<_>, Vec<_>) =
                    sources.drain(..).partition(|(id, _)| wanted_sources.contains(id));
                *sources = keep;
                drop
            };
            drop(removed);
            for id in wanted_sources {
                if !sources.iter().any(|(existing, _)| *existing == id) {
                    sources.push((id.clone(), Capture::start(id)));
                }
            }
        }

        // `None` is the default output, followed as Windows changes it.
        let wanted_outputs: Vec<Option<String>> = if config.receive_enabled {
            config.outputs.iter().map(|id| (id != profile::DEFAULT_OUTPUT_ID).then(|| id.clone())).collect()
        } else {
            Vec::new()
        };
        let volume = config.volume.min(100) as f32 / 100.0;
        let params = shared.buffer_params(config);
        {
            let mut outputs = shared.outputs.lock().unwrap();
            let (keep, removed): (Vec<_>, Vec<_>) =
                outputs.drain(..).partition(|(id, _)| wanted_outputs.contains(id));
            *outputs = keep;
            drop(removed);
            for id in wanted_outputs {
                if !outputs.iter().any(|(existing, _)| *existing == id) {
                    outputs.push((id.clone(), Output::start(id, params)));
                }
            }
            for (_, output) in outputs.iter() {
                let mut mix = output.mix.lock().unwrap();
                mix.set_params(params);
                mix.volume = volume;
            }
        }

        shared.discovery.set_announcement(announcement(config));
    }

    pub fn snapshot(&self) -> Snapshot {
        let shared = &self.shared;
        let config = shared.config.lock().unwrap().clone();
        let discovered = shared.discovery.peers();
        let now = Instant::now();
        let peers = {
            let states = shared.peers.lock().unwrap();
            config
                .peers
                .iter()
                .filter_map(|target| {
                    let state = states.get(&target.address)?;
                    let health = match state.last_pong {
                        Some(t) if now - t < HEALTHY_WINDOW => Health::Healthy,
                        Some(t) if now - t < UNREACHABLE_WINDOW => Health::Stale,
                        Some(_) => Health::Unreachable,
                        None if now - state.added < UNREACHABLE_WINDOW => Health::Connecting,
                        None => Health::Unreachable,
                    };
                    let found = discovered.iter().find(|d| d.address == target.address);
                    let name = match (target.name.trim(), found) {
                        ("", Some(d)) => d.name.clone(),
                        ("", None) => target.address.to_string(),
                        (name, _) => name.to_string(),
                    };
                    Some(PeerSnapshot {
                        address: target.address,
                        name,
                        connected_for: state.first_pong.filter(|_| health != Health::Unreachable).map(|t| now - t),
                        rtt_ms: state.rtt_ms.filter(|_| health != Health::Unreachable),
                        health,
                        their_format: state.their_format.clone(),
                        receiving_their_audio: state.last_audio.is_some_and(|t| now - t < Duration::from_secs(3)),
                        password_mismatch: state.password_mismatch,
                        can_receive: found.map(|d| d.can_receive),
                    })
                })
                .collect()
        };
        let (sources_running, source_errors) = {
            let sources = shared.sources.lock().unwrap();
            let running = sources.iter().filter(|(_, c)| c.queue.running.load(Ordering::Relaxed)).count();
            let errors = sources
                .iter()
                .filter(|(_, c)| !c.queue.running.load(Ordering::Relaxed))
                .filter_map(|(_, c)| c.queue.error.lock().unwrap().clone())
                .collect();
            (running, errors)
        };
        let (output_errors, output_latency_ms) = {
            let outputs = shared.outputs.lock().unwrap();
            let errors = outputs
                .iter()
                .filter(|(_, o)| !o.running.load(Ordering::Relaxed))
                .filter_map(|(_, o)| o.error.lock().unwrap().clone())
                .collect();
            let latency = outputs.iter().map(|(_, o)| *o.latency_ms.lock().unwrap()).fold(0.0, f32::max);
            (errors, latency)
        };
        let last_send = shared.last_send_ms.load(Ordering::Relaxed);
        Snapshot {
            uptime: shared.started.elapsed(),
            bytes_received: shared.bytes_received.load(Ordering::Relaxed),
            bytes_sent: shared.bytes_sent.load(Ordering::Relaxed),
            port_error: shared.port_error.lock().unwrap().clone(),
            discovery_error: shared.discovery.error(),
            keys_ready: shared.keys.lock().unwrap().as_ref().is_some_and(|(p, _)| *p == config.password),
            peers,
            discovered,
            incoming: {
                let incoming = shared.incoming.lock().unwrap();
                let mut list: Vec<IncomingPeer> = incoming
                    .iter()
                    .filter(|(address, (_, seen))| now - *seen < INCOMING_WINDOW && !config.is_peer(**address))
                    .map(|(address, (instance_id, _))| IncomingPeer { address: *address, instance_id: *instance_id })
                    .collect();
                list.sort_by_key(|p| p.address);
                list
            },
            sources_running,
            source_errors,
            output_errors,
            output_latency_ms,
            jitter_ms: shared.tuned_jitter_ms.load(Ordering::Relaxed),
            sending: last_send > 0 && (shared.now_ms() as u64).saturating_sub(last_send) < 1000,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.shared.sources.lock().unwrap().clear();
        self.shared.outputs.lock().unwrap().clear();
        *self.shared.socket.lock().unwrap() = None;
    }
}

fn announcement(config: &EngineConfig) -> Announcement {
    Announcement {
        name: config.name.clone(),
        can_send: config.send_enabled,
        can_receive: config.receive_enabled,
        unicast: config.peers.iter().map(|p| p.address).collect(),
    }
}

// ================================================================== receiving

struct RecvSession {
    format: AudioFormat,
    decoder: Option<opus::Decoder>,
    expected_next: Option<u32>,
    /// PCM frames still being assembled: frame id -> parts.
    pcm_parts: HashMap<u32, Vec<Option<Vec<u8>>>>,
}

fn net_loop(shared: &Shared) {
    let mut sessions: HashMap<StreamKey, RecvSession> = HashMap::new();
    let mut opener: Option<([u8; 8], Opener)> = None;
    let mut seen_proofs: HashSet<[u8; 12]> = HashSet::new();
    let mut buf = [0u8; 2048];
    let mut pcm: Vec<f32> = Vec::with_capacity(5760 * 2);
    while !shared.stop.load(Ordering::Relaxed) {
        let socket = match shared.socket() {
            Some(socket) => socket,
            None => match bind_audio_port() {
                Ok(socket) => {
                    *shared.port_error.lock().unwrap() = None;
                    let socket = Arc::new(socket);
                    *shared.socket.lock().unwrap() = Some(socket.clone());
                    socket
                }
                Err(e) => {
                    *shared.port_error.lock().unwrap() = Some(e);
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            },
        };
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok((n, SocketAddr::V4(from))) => (n, from),
            _ => continue,
        };
        let packet = &buf[..n];
        let address = *from.ip();
        let (is_peer, config_receive) = {
            let config = shared.config.lock().unwrap();
            (config.is_peer(address), config.receive_enabled)
        };
        let Some(header) = Header::parse(packet) else { continue };
        let payload = &packet[HEADER_SIZE..];
        if !is_peer {
            // Someone we haven't ticked: answer pings (as RemSound does, so it can see we're
            // here) and note a valid tick proof as a request to connect. Nothing else.
            match header.packet_type {
                PacketType::Heartbeat if header.stream_id == CONTROL_STREAM_ID => {
                    if let Some((packet::HEARTBEAT_PING, stamp)) = packet::parse_heartbeat(payload) {
                        let mut reply = Vec::with_capacity(HEADER_SIZE + 9);
                        Header::new(PacketType::Heartbeat, CONTROL_STREAM_ID, header.sequence).write(&mut reply);
                        packet::write_heartbeat(&mut reply, packet::HEARTBEAT_PONG, stamp);
                        let _ = socket.send_to(&reply, from);
                    }
                }
                PacketType::TickProof => {
                    let Some(keys) = shared.keys() else { continue };
                    if opener.as_ref().is_none_or(|(fp, _)| *fp != keys.fingerprint) {
                        opener = Some((keys.fingerprint, Opener::new(&keys.key)));
                        seen_proofs.clear();
                    }
                    if let Some(instance_id) = check_tick_proof(&opener.as_ref().unwrap().1, payload, &mut seen_proofs) {
                        shared.incoming.lock().unwrap().insert(address, (instance_id, Instant::now()));
                    }
                }
                _ => {}
            }
            continue;
        }
        shared.bytes_received.fetch_add(n as u64, Ordering::Relaxed);
        match header.packet_type {
            PacketType::Heartbeat if header.stream_id == CONTROL_STREAM_ID => {
                let Some((kind, stamp)) = packet::parse_heartbeat(payload) else { continue };
                if kind == packet::HEARTBEAT_PING {
                    let mut reply = Vec::with_capacity(HEADER_SIZE + 9);
                    Header::new(PacketType::Heartbeat, CONTROL_STREAM_ID, header.sequence).write(&mut reply);
                    packet::write_heartbeat(&mut reply, packet::HEARTBEAT_PONG, stamp);
                    if socket.send_to(&reply, from).is_ok() {
                        shared.bytes_sent.fetch_add(reply.len() as u64, Ordering::Relaxed);
                    }
                } else {
                    let rtt = shared.now_ms() - stamp;
                    if !(0..10_000).contains(&rtt) {
                        continue; // someone else's clock (a relay group), not an answer to us
                    }
                    let mut peers = shared.peers.lock().unwrap();
                    if let Some(state) = peers.get_mut(&address) {
                        let now = Instant::now();
                        if state.first_pong.is_none()
                            || state.last_pong.is_none_or(|t| now - t >= UNREACHABLE_WINDOW)
                        {
                            state.first_pong = Some(now);
                        }
                        state.last_pong = Some(now);
                        let rtt = rtt as u32;
                        state.rtt_ms = Some(match state.rtt_ms {
                            Some(old) => (old * 7 + rtt * 3) / 10,
                            None => rtt,
                        });
                    }
                }
            }
            PacketType::AddrCheck => {
                // A relay proving our address: echo it back exactly.
                let _ = socket.send_to(packet, from);
            }
            PacketType::Format => {
                if header.stream_id == HELLO_STREAM_ID || header.stream_id == CONTROL_STREAM_ID {
                    continue;
                }
                let Some(format) = AudioFormat::parse(payload) else { continue };
                let keys = shared.keys();
                let mismatch = match (&format.fingerprint, &keys) {
                    (Some(theirs), Some(ours)) => *theirs != ours.fingerprint,
                    _ => false,
                };
                {
                    let mut peers = shared.peers.lock().unwrap();
                    if let Some(state) = peers.get_mut(&address) {
                        state.password_mismatch = mismatch;
                        state.their_format = Some(format.clone());
                    }
                }
                if mismatch || (format.codec != CODEC_OPUS && format.codec != CODEC_PCM) {
                    sessions.remove(&(address, header.stream_id));
                    continue;
                }
                let key = (address, header.stream_id);
                if sessions.get(&key).is_some_and(|s| s.format.same_stream(&format)) {
                    continue;
                }
                // A peer runs one stream at a time: a new stream id replaces its old one.
                sessions.retain(|(a, _), _| *a != address);
                let decoder = if format.codec == CODEC_OPUS { opus::Decoder::new().ok() } else { None };
                sessions.insert(key, RecvSession { format, decoder, expected_next: None, pcm_parts: HashMap::new() });
            }
            PacketType::Audio => {
                if !config_receive {
                    continue;
                }
                let key = (address, header.stream_id);
                let Some(session) = sessions.get_mut(&key) else { continue };
                let Some(keys) = shared.keys() else { continue };
                if opener.as_ref().is_none_or(|(fp, _)| *fp != keys.fingerprint) {
                    opener = Some((keys.fingerprint, Opener::new(&keys.key)));
                }
                let opener = &opener.as_ref().unwrap().1;
                let decoded = if session.format.codec == CODEC_OPUS {
                    decode_opus(session, header.sequence, payload, opener, &mut pcm, |samples| {
                        write_to_outputs(shared, key, samples)
                    })
                } else {
                    decode_pcm(session, payload, opener, &mut pcm)
                        .map(|_| write_to_outputs(shared, key, &pcm))
                        .is_some()
                };
                if decoded
                    && let Some(state) = shared.peers.lock().unwrap().get_mut(&address) {
                        state.last_audio = Some(Instant::now());
                    }
            }
            _ => {}
        }
    }
}

/// Opens a tick proof and returns the sender's instance id if it is genuine: sealed with our
/// key, recent, and never seen before. A replayed proof proves nothing, and accepting one
/// could make us stream to whoever replayed it.
fn check_tick_proof(opener: &Opener, payload: &[u8], seen: &mut HashSet<[u8; 12]>) -> Option<Uuid> {
    let plain = opener.open(payload)?;
    if plain.len() != 25 || plain[0] != 1 {
        return None;
    }
    let sent = i64::from_le_bytes(plain[1..9].try_into().ok()?);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    if (now - sent).abs() > TICK_PROOF_SKEW_SECS {
        return None;
    }
    let nonce: [u8; 12] = payload[..12].try_into().ok()?;
    if seen.len() > 8192 {
        seen.clear();
    }
    if !seen.insert(nonce) {
        return None;
    }
    Some(Uuid::from_bytes(plain[9..25].try_into().ok()?))
}

fn write_to_outputs(shared: &Shared, key: StreamKey, samples: &[f32]) {
    for (_, output) in shared.outputs.lock().unwrap().iter() {
        output.mix.lock().unwrap().write(key, samples);
    }
}

/// Opus: open, rebuild a single lost packet from the FEC copy the next one carries, decode.
fn decode_opus(
    session: &mut RecvSession,
    sequence: u32,
    payload: &[u8],
    opener: &Opener,
    pcm: &mut Vec<f32>,
    mut write: impl FnMut(&[f32]),
) -> bool {
    let Some(plain) = opener.open(payload) else { return false };
    let frame_samples = (session.format.frame_samples as i64 * 48_000 / session.format.sample_rate.max(1) as i64)
        .clamp(120, 5760) as i32;
    let Some(decoder) = session.decoder.as_mut() else { return false };
    if let Some(expected) = session.expected_next {
        let gap = sequence.wrapping_sub(expected);
        if gap > 1_000_000 {
            return false; // late or repeated packet
        }
        if gap == 1 && decoder.decode(Some(&plain), frame_samples, true, pcm).is_ok() {
            write(pcm);
        }
    }
    session.expected_next = Some(sequence.wrapping_add(1));
    if decoder.decode(Some(&plain), frame_samples, false, pcm).is_ok() {
        write(pcm);
        true
    } else {
        false
    }
}

/// PCM: collect a frame's parts, open the whole ciphertext, unpack 24-bit samples.
fn decode_pcm(session: &mut RecvSession, payload: &[u8], opener: &Opener, pcm: &mut Vec<f32>) -> Option<()> {
    let (frame_id, part, parts) = packet::parse_pcm_subheader(payload)?;
    let data = payload[packet::PCM_SUBHEADER_SIZE..].to_vec();
    let complete = {
        let slots = session.pcm_parts.entry(frame_id).or_insert_with(|| vec![None; parts as usize]);
        if slots.len() != parts as usize {
            *slots = vec![None; parts as usize];
        }
        slots[part as usize] = Some(data);
        slots.iter().all(Option::is_some)
    };
    // Forget frames that will never complete.
    session.pcm_parts.retain(|id, _| frame_id.wrapping_sub(*id) < 16);
    if !complete {
        return None;
    }
    let sealed: Vec<u8> = session.pcm_parts.remove(&frame_id)?.into_iter().flatten().flatten().collect();
    let plain = opener.open(&sealed)?;
    let channels = session.format.channels.clamp(1, 2) as usize;
    pcm.clear();
    for frame in plain.chunks_exact(3 * channels) {
        let mut values = [0.0f32; 2];
        for (channel, value) in values.iter_mut().enumerate().take(channels) {
            let b = &frame[channel * 3..channel * 3 + 3];
            let raw = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
            *value = raw as f32 / 8_388_608.0;
        }
        if channels == 1 {
            values[1] = values[0];
        }
        pcm.extend_from_slice(&values);
    }
    Some(())
}

// ================================================================== timers

fn timer_loop(shared: &Shared) {
    let mut last_ping: Option<Instant> = None;
    let mut last_proof: Option<Instant> = None;
    let mut last_tune = Instant::now();
    let mut quiet_since = Instant::now();
    let mut last_underruns = 0u64;
    let mut ping_sequence = 0u32;
    let mut proof_sequence = 0u32;
    let mut proof_sealer: Option<([u8; 8], Sealer)> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(50));
        let now = Instant::now();
        let Some(socket) = shared.socket() else { continue };
        let config = shared.config.lock().unwrap().clone();

        if last_ping.is_none_or(|t| now - t >= PING_INTERVAL) {
            last_ping = Some(now);
            for peer in &config.peers {
                ping_sequence = ping_sequence.wrapping_add(1);
                let mut ping = Vec::with_capacity(HEADER_SIZE + 9);
                Header::new(PacketType::Heartbeat, CONTROL_STREAM_ID, ping_sequence).write(&mut ping);
                packet::write_heartbeat(&mut ping, packet::HEARTBEAT_PING, shared.now_ms());
                shared.send_to(&socket, &ping, peer.address);
            }
        }

        if last_proof.is_none_or(|t| now - t >= TICK_PROOF_INTERVAL)
            && let Some(keys) = shared.keys() {
                last_proof = Some(now);
                if proof_sealer.as_ref().is_none_or(|(fp, _)| *fp != keys.fingerprint) {
                    proof_sealer = Some((keys.fingerprint, Sealer::new(&keys.key)));
                }
                let sealer = &mut proof_sealer.as_mut().unwrap().1;
                for peer in &config.peers {
                    proof_sequence = proof_sequence.wrapping_add(1);
                    let proof = crate::protocol::tick_proof(sealer, shared.instance_id, proof_sequence);
                    shared.send_to(&socket, &proof, peer.address);
                }
            }

        if now - last_tune >= AUTO_TUNE_INTERVAL {
            last_tune = now;
            let outputs = shared.outputs.lock().unwrap();
            let underruns: u64 = outputs
                .iter()
                .map(|(_, o)| o.mix.lock().unwrap().streams.values().map(|s| s.buffer.underruns).sum::<u64>())
                .sum();
            if config.auto_tune {
                let current = shared.tuned_jitter_ms.load(Ordering::Relaxed);
                let next = if underruns > last_underruns {
                    quiet_since = now;
                    (current + 10).min(profile::MAX_JITTER_MS)
                } else if now - quiet_since >= Duration::from_secs(60) {
                    quiet_since = now;
                    current.saturating_sub(5).max(profile::MIN_JITTER_MS)
                } else {
                    current
                };
                if next != current {
                    shared.tuned_jitter_ms.store(next, Ordering::Relaxed);
                    let params = shared.buffer_params(&config);
                    for (_, output) in outputs.iter() {
                        output.mix.lock().unwrap().set_params(params);
                    }
                }
            }
            last_underruns = underruns;
        }
    }
}

// ================================================================== sending

struct Encoding {
    codec: Codec,
    frame_samples: usize,
    stream_id: u16,
    sequence: u32,
    format_sequence: u32,
    pcm_frame_id: u32,
    opus: Option<opus::Encoder>,
    sealer: Sealer,
    fingerprint: [u8; 8],
    last_format: Option<Instant>,
}

impl Encoding {
    /// Seals one frame into the datagrams that carry it: one for Opus, one or more for PCM.
    fn packets(&mut self, frame: &[f32]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        match self.codec {
            Codec::Pcm24 => {
                let mut plain = Vec::with_capacity(frame.len() * 3);
                for sample in frame {
                    let value = (sample.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
                    plain.extend_from_slice(&value.to_le_bytes()[..3]);
                }
                let mut sealed = Vec::with_capacity(plain.len() + OVERHEAD);
                self.sealer.seal(&plain, &mut sealed);
                let parts = sealed.len().div_ceil(MAX_AUDIO_PAYLOAD);
                self.pcm_frame_id = self.pcm_frame_id.wrapping_add(1);
                for (index, chunk) in sealed.chunks(MAX_AUDIO_PAYLOAD).enumerate() {
                    self.sequence = self.sequence.wrapping_add(1);
                    let mut datagram = Vec::with_capacity(HEADER_SIZE + packet::PCM_SUBHEADER_SIZE + chunk.len());
                    Header::new(PacketType::Audio, self.stream_id, self.sequence).write(&mut datagram);
                    packet::write_pcm_subheader(&mut datagram, self.pcm_frame_id, index as u8, parts as u8);
                    datagram.extend_from_slice(chunk);
                    out.push(datagram);
                }
            }
            _ => {
                let Some(opus) = self.opus.as_mut() else { return out };
                let mut encoded = Vec::with_capacity(1500);
                if opus.encode(frame, &mut encoded).is_err() {
                    return out;
                }
                self.sequence = self.sequence.wrapping_add(1);
                let mut datagram = Vec::with_capacity(HEADER_SIZE + encoded.len() + OVERHEAD);
                Header::new(PacketType::Audio, self.stream_id, self.sequence).write(&mut datagram);
                self.sealer.seal(&encoded, &mut datagram);
                out.push(datagram);
            }
        }
        out
    }
}

fn random_stream_id() -> u16 {
    loop {
        let id: u16 = rand::random();
        if id > 1 && id < HELLO_STREAM_ID {
            return id;
        }
    }
}

fn sender_loop(shared: &Shared) {
    // 1 ms timer resolution, so the 2 ms pacing below holds.
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
    let mut config = shared.config.lock().unwrap().clone();
    let mut generation = shared.config_generation.load(Ordering::Relaxed);
    let mut encoding: Option<Encoding> = None;
    let mut clock: Option<(Instant, u64)> = None; // (start, frames mixed since)
    let mut primed: HashMap<String, bool> = HashMap::new();
    let mut accumulator: Vec<f32> = Vec::with_capacity(48_000);
    let mut mix: Vec<f32> = Vec::with_capacity(48_000);
    let mut datagram = Vec::with_capacity(1600);
    while !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(2));
        let current = shared.config_generation.load(Ordering::Relaxed);
        if current != generation {
            generation = current;
            config = shared.config.lock().unwrap().clone();
        }
        let socket = shared.socket();
        let keys = shared.keys();
        let sources_running = shared.sources.lock().unwrap().iter().any(|(_, c)| c.queue.running.load(Ordering::Relaxed));
        let (Some(socket), Some(keys)) = (socket, keys) else {
            clock = None;
            continue;
        };
        if !config.send_enabled || config.peers.is_empty() || !sources_running {
            clock = None;
            accumulator.clear();
            continue;
        }

        // (Re)build the encoder when the codec, packet size or password changed.
        let frame_samples = profile::frame_samples(config.codec, config.packet_size) as usize;
        let rebuild = encoding.as_ref().is_none_or(|e| {
            e.codec != config.codec || e.frame_samples != frame_samples || e.fingerprint != keys.fingerprint
        });
        if rebuild {
            let opus = match config.codec {
                Codec::Pcm24 => None,
                _ => match opus::Encoder::new(frame_samples as i32, OPUS_BITRATE) {
                    Ok(encoder) => Some(encoder),
                    Err(_) => continue,
                },
            };
            encoding = Some(Encoding {
                codec: config.codec,
                frame_samples,
                stream_id: random_stream_id(),
                sequence: 0,
                format_sequence: 0,
                pcm_frame_id: 0,
                opus,
                sealer: Sealer::new(&keys.key),
                fingerprint: keys.fingerprint,
                last_format: None,
            });
            accumulator.clear();
        }
        let encoding = encoding.as_mut().unwrap();

        let now = Instant::now();
        if encoding.last_format.is_none_or(|t| now - t >= FORMAT_INTERVAL) {
            encoding.last_format = Some(now);
            let capture_ms = shared
                .sources
                .lock()
                .unwrap()
                .iter()
                .map(|(_, c)| *c.queue.latency_ms.lock().unwrap())
                .fold(0.0, f32::max);
            let format = match encoding.codec {
                Codec::Pcm24 => AudioFormat::pcm24(frame_samples as i32, keys.fingerprint, capture_ms),
                _ => AudioFormat::opus(frame_samples as i32, keys.fingerprint, capture_ms),
            };
            encoding.format_sequence = encoding.format_sequence.wrapping_add(1);
            datagram.clear();
            Header::new(PacketType::Format, encoding.stream_id, encoding.format_sequence).write(&mut datagram);
            format.write(&mut datagram);
            for peer in &config.peers {
                shared.send_to(&socket, &datagram, peer.address);
            }
        }

        // How many frames the system clock says are due since the stream started.
        let (start, mixed) = clock.get_or_insert((now, 0));
        let due_total = (now - *start).as_micros() as u64 * 48 / 1000;
        let mut due = due_total.saturating_sub(*mixed) as usize;
        if due > 4800 {
            // A stall (the machine was busy): skip ahead rather than send a burst.
            *mixed = due_total - 480;
            due = 480;
        }
        if due == 0 {
            continue;
        }
        *mixed += due as u64;

        mix.clear();
        mix.resize(due * 2, 0.0);
        for (id, capture) in shared.sources.lock().unwrap().iter() {
            let mut queue = capture.queue.samples.lock().unwrap();
            let is_primed = primed.entry(id.clone()).or_insert(false);
            // Each source keeps 10 ms in hand so its own timing wobble never reaches the mix.
            if !*is_primed {
                if queue.len() >= 960 {
                    *is_primed = true;
                } else {
                    continue;
                }
            }
            let take = queue.len().min(mix.len());
            for (slot, sample) in mix.iter_mut().zip(queue.drain(..take)) {
                *slot += sample;
            }
            if take < mix.len() {
                *is_primed = false;
            }
            // Drift the other way: keep no more than 60 ms waiting.
            if queue.len() > 48 * 60 * 2 {
                let excess = queue.len() - 48 * 20 * 2;
                queue.drain(..excess);
            }
        }
        accumulator.extend_from_slice(&mix);

        let frame_len = frame_samples * 2;
        while accumulator.len() >= frame_len {
            let frame: Vec<f32> = accumulator.drain(..frame_len).collect();
            for datagram in encoding.packets(&frame) {
                for peer in &config.peers {
                    shared.send_to(&socket, &datagram, peer.address);
                }
            }
            shared.last_send_ms.store(shared.now_ms().max(1) as u64, Ordering::Relaxed);
        }
    }
    unsafe {
        windows::Win32::Media::timeEndPeriod(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoding(codec: Codec, frame_samples: usize, keys: &Keys) -> Encoding {
        Encoding {
            codec,
            frame_samples,
            stream_id: 7,
            sequence: 0,
            format_sequence: 0,
            pcm_frame_id: 0,
            opus: (codec != Codec::Pcm24).then(|| opus::Encoder::new(frame_samples as i32, OPUS_BITRATE).unwrap()),
            sealer: Sealer::new(&keys.key),
            fingerprint: keys.fingerprint,
            last_format: None,
        }
    }

    fn session(format: AudioFormat) -> RecvSession {
        let decoder = (format.codec == CODEC_OPUS).then(|| opus::Decoder::new().unwrap());
        RecvSession { format, decoder, expected_next: None, pcm_parts: HashMap::new() }
    }

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames * 2).map(|i| ((i / 2) as f32 / frames as f32) - 0.5).collect()
    }

    #[test]
    fn pcm_frames_survive_sealing_splitting_and_reassembly() {
        let keys = Keys::derive("pw");
        let opener = Opener::new(&keys.key);
        for frame_samples in [233usize, 240, 120] {
            let mut sender = encoding(Codec::Pcm24, frame_samples, &keys);
            let mut receiver = session(AudioFormat::pcm24(frame_samples as i32, keys.fingerprint, 0.0));
            let frame = ramp(frame_samples);
            let datagrams = sender.packets(&frame);
            // 233 samples is the largest frame that still fits one packet; 240 needs two.
            assert_eq!(datagrams.len(), if frame_samples == 240 { 2 } else { 1 });
            assert!(datagrams.iter().all(|d| d.len() <= HEADER_SIZE + packet::PCM_SUBHEADER_SIZE + MAX_AUDIO_PAYLOAD));
            let mut pcm = Vec::new();
            let mut done = false;
            // Parts may arrive in any order.
            for datagram in datagrams.iter().rev() {
                let header = Header::parse(datagram).unwrap();
                assert_eq!(header.packet_type, PacketType::Audio);
                done = decode_pcm(&mut receiver, &datagram[HEADER_SIZE..], &opener, &mut pcm).is_some();
            }
            assert!(done);
            assert_eq!(pcm.len(), frame.len());
            for (a, b) in pcm.iter().zip(&frame) {
                assert!((a - b).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn opus_frames_decode_and_a_lost_packet_is_rebuilt() {
        let keys = Keys::derive("pw");
        let opener = Opener::new(&keys.key);
        let mut sender = encoding(Codec::OpusLive, 480, &keys);
        let mut receiver = session(AudioFormat::opus(480, keys.fingerprint, 0.0));
        let mut written = 0usize;
        let mut pcm = Vec::new();
        for index in 0..10 {
            let datagrams = sender.packets(&ramp(480));
            assert_eq!(datagrams.len(), 1);
            if index == 5 {
                continue; // lost on the way
            }
            let header = Header::parse(&datagrams[0]).unwrap();
            assert!(decode_opus(&mut receiver, header.sequence, &datagrams[0][HEADER_SIZE..], &opener, &mut pcm, |s| {
                written += s.len()
            }));
        }
        // Nine packets arrived, and the tenth frame's worth came back through FEC.
        assert_eq!(written, 10 * 480 * 2);
        let wrong = Opener::new(&Keys::derive("other").key);
        let datagram = sender.packets(&ramp(480)).remove(0);
        let header = Header::parse(&datagram).unwrap();
        assert!(!decode_opus(&mut receiver, header.sequence, &datagram[HEADER_SIZE..], &wrong, &mut pcm, |_| {}));
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Ticks 127.0.0.1 and talks to itself over the real port: heartbeats must come back as a
    /// healthy link with a round-trip time. Ignored by default because it binds UDP 47830 and
    /// announces this machine on the LAN for a few seconds: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn heartbeats_over_the_real_port() {
        let mut profile = Profile { password: "pw".into(), ..Default::default() };
        profile.peers.push(crate::profile::SavedPeer { name: "me".into(), address: "127.0.0.1".into(), active: true });
        profile.receive_enabled = false;
        let engine = Engine::start(EngineConfig::from_profile(&profile, true));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = engine.snapshot();
            assert!(snapshot.port_error.is_none(), "{:?}", snapshot.port_error);
            let peer = &snapshot.peers[0];
            if peer.health == Health::Healthy && peer.rtt_ms.is_some() && snapshot.keys_ready {
                assert!(snapshot.bytes_sent > 0 && snapshot.bytes_received > 0);
                break;
            }
            assert!(Instant::now() < deadline, "no healthy link: {peer:?}");
            thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tick_proof_tests {
    use super::*;

    #[test]
    fn genuine_proofs_pass_once_and_others_never() {
        let keys = Keys::derive("pw");
        let opener = Opener::new(&keys.key);
        let mut sealer = Sealer::new(&keys.key);
        let id = Uuid::new_v4();
        let packet = crate::protocol::tick_proof(&mut sealer, id, 1);
        let payload = &packet[HEADER_SIZE..];
        let mut seen = HashSet::new();
        assert_eq!(check_tick_proof(&opener, payload, &mut seen), Some(id));
        assert_eq!(check_tick_proof(&opener, payload, &mut seen), None, "a replay is refused");
        let other = Opener::new(&Keys::derive("other").key);
        let fresh = crate::protocol::tick_proof(&mut sealer, id, 2);
        assert_eq!(check_tick_proof(&other, &fresh[HEADER_SIZE..], &mut HashSet::new()), None);
    }
}
