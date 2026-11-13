//! RemSound's LAN/VPN peer discovery: every copy announces itself as a small JSON object on
//! UDP port 47821 every 1.5 s, by broadcast on each local subnet and by unicast to the
//! addresses it knows (broadcast doesn't cross a VPN such as Tailscale). Peers that go
//! quiet for 8 s are dropped.
//!
//! The JSON uses .NET's default property names: `InstanceId`, `Name`, `AudioPort`,
//! `CanSend`, `CanReceive`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DISCOVERY_PORT: u16 = 47821;
const ANNOUNCE_INTERVAL: Duration = Duration::from_millis(1500);
const PEER_EXPIRY: Duration = Duration::from_secs(8);
const HEARD_FROM_EXPIRY: Duration = Duration::from_secs(30);
const MAX_HEARD_FROM: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Message {
    instance_id: Uuid,
    name: String,
    audio_port: i32,
    can_send: bool,
    can_receive: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredPeer {
    pub instance_id: Uuid,
    pub name: String,
    pub address: Ipv4Addr,
    pub audio_port: u16,
    pub can_send: bool,
    pub can_receive: bool,
}

/// What this copy says about itself. Updated live as sending and receiving are toggled.
#[derive(Clone, Debug)]
pub struct Announcement {
    pub name: String,
    pub can_send: bool,
    pub can_receive: bool,
    /// Announced to directly as well as by broadcast: the peers the user has added.
    pub unicast: Vec<Ipv4Addr>,
}

struct Shared {
    instance_id: Uuid,
    announcement: Mutex<Announcement>,
    peers: Mutex<HashMap<Uuid, (DiscoveredPeer, Instant)>>,
    heard_from: Mutex<HashMap<Ipv4Addr, Instant>>,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

pub struct Discovery {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

fn parse(payload: &[u8], from: Ipv4Addr, own_id: Uuid) -> Option<DiscoveredPeer> {
    let message: Message = serde_json::from_slice(payload).ok()?;
    if message.instance_id == own_id || message.instance_id.is_nil() {
        return None;
    }
    let audio_port = u16::try_from(message.audio_port).ok().filter(|p| *p > 0)?;
    let mut name = message.name.trim().to_string();
    if name.is_empty() {
        name = from.to_string();
    }
    name = name.chars().take(128).collect();
    Some(DiscoveredPeer {
        instance_id: message.instance_id,
        name,
        address: from,
        audio_port,
        can_send: message.can_send,
        can_receive: message.can_receive,
    })
}

fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if interface.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = interface.addr {
                let broadcast = v4.broadcast.unwrap_or_else(|| {
                    Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask))
                });
                if !out.contains(&broadcast) {
                    out.push(broadcast);
                }
            }
        }
    }
    out
}

fn bind_listener() -> Result<UdpSocket, String> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    let _ = socket.set_reuse_address(true);
    let _ = socket.set_broadcast(true);
    let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT));
    socket
        .bind(&address.into())
        .map_err(|e| format!("discovery port {DISCOVERY_PORT}: {e}"))?;
    let socket: UdpSocket = socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(250))).map_err(|e| e.to_string())?;
    Ok(socket)
}

impl Discovery {
    pub fn start(instance_id: Uuid, announcement: Announcement) -> Self {
        let shared = Arc::new(Shared {
            instance_id,
            announcement: Mutex::new(announcement),
            peers: Mutex::new(HashMap::new()),
            heard_from: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let mut threads = Vec::new();

        let listen = shared.clone();
        threads.push(thread::spawn(move || {
            let socket = loop {
                if listen.stop.load(Ordering::Relaxed) {
                    return;
                }
                match bind_listener() {
                    Ok(socket) => {
                        *listen.error.lock().unwrap() = None;
                        break socket;
                    }
                    Err(e) => {
                        *listen.error.lock().unwrap() = Some(e);
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            };
            let mut buf = [0u8; 2048];
            while !listen.stop.load(Ordering::Relaxed) {
                let Ok((n, SocketAddr::V4(from))) = socket.recv_from(&mut buf) else { continue };
                let Some(peer) = parse(&buf[..n], *from.ip(), listen.instance_id) else { continue };
                let now = Instant::now();
                {
                    let mut heard = listen.heard_from.lock().unwrap();
                    heard.insert(peer.address, now);
                    if heard.len() > MAX_HEARD_FROM
                        && let Some(oldest) = heard.iter().min_by_key(|(_, t)| **t).map(|(a, _)| *a) {
                            heard.remove(&oldest);
                        }
                }
                listen.peers.lock().unwrap().insert(peer.instance_id, (peer, now));
            }
        }));

        let announce = shared.clone();
        threads.push(thread::spawn(move || {
            let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) else { return };
            let _ = socket.set_broadcast(true);
            let mut broadcasts = broadcast_addresses();
            let mut last_refresh = Instant::now();
            let mut last_sent: Option<Instant> = None;
            while !announce.stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if last_sent.is_none_or(|t| now - t >= ANNOUNCE_INTERVAL) {
                    last_sent = Some(now);
                    if now - last_refresh > Duration::from_secs(30) {
                        broadcasts = broadcast_addresses();
                        last_refresh = now;
                    }
                    let current = announce.announcement.lock().unwrap().clone();
                    let message = Message {
                        instance_id: announce.instance_id,
                        name: current.name.clone(),
                        audio_port: crate::protocol::packet::AUDIO_PORT as i32,
                        can_send: current.can_send,
                        can_receive: current.can_receive,
                    };
                    let bytes = serde_json::to_vec(&message).unwrap_or_default();
                    let mut targets = broadcasts.clone();
                    targets.extend(current.unicast.iter().copied());
                    {
                        let mut heard = announce.heard_from.lock().unwrap();
                        heard.retain(|_, t| now - *t <= HEARD_FROM_EXPIRY);
                        targets.extend(heard.keys().copied());
                    }
                    targets.sort();
                    targets.dedup();
                    for target in targets {
                        let _ = socket.send_to(&bytes, SocketAddrV4::new(target, DISCOVERY_PORT));
                    }
                    announce.peers.lock().unwrap().retain(|_, (_, seen)| now - *seen <= PEER_EXPIRY);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }));

        Self { shared, threads }
    }

    pub fn set_announcement(&self, announcement: Announcement) {
        *self.shared.announcement.lock().unwrap() = announcement;
    }

    /// Everyone heard within the last 8 seconds, sorted by name.
    pub fn peers(&self) -> Vec<DiscoveredPeer> {
        let now = Instant::now();
        let mut peers: Vec<DiscoveredPeer> = self
            .shared
            .peers
            .lock()
            .unwrap()
            .values()
            .filter(|(_, seen)| now - *seen <= PEER_EXPIRY)
            .map(|(peer, _)| peer.clone())
            .collect();
        peers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.address.cmp(&b.address)));
        peers
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remsound_announcements() {
        let own = Uuid::new_v4();
        let json = br#"{"InstanceId":"0f8fad5b-d9cb-469f-a165-70867728950e","Name":" hermes ","AudioPort":47830,"CanSend":true,"CanReceive":false}"#;
        let peer = parse(json, Ipv4Addr::new(10, 0, 0, 2), own).unwrap();
        assert_eq!(peer.name, "hermes");
        assert_eq!(peer.audio_port, 47830);
        assert!(peer.can_send && !peer.can_receive);
        let bad_port = br#"{"InstanceId":"0f8fad5b-d9cb-469f-a165-70867728950e","Name":"x","AudioPort":99999,"CanSend":true,"CanReceive":true}"#;
        assert!(parse(bad_port, Ipv4Addr::LOCALHOST, own).is_none());
        let message = Message { instance_id: own, name: "me".into(), audio_port: 47830, can_send: true, can_receive: true };
        let text = serde_json::to_string(&message).unwrap();
        assert!(text.contains("\"InstanceId\"") && text.contains("\"CanReceive\":true"));
        assert!(parse(text.as_bytes(), Ipv4Addr::LOCALHOST, own).is_none());
    }
}
