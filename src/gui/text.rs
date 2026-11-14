//! The words the window shows: peer details, connection status and total latency.
//! Kept free of wx so it can be tested.
//!
//! Each builder returns the text and a "stable key": the part that doesn't tick over every
//! second. A focused read-only field is only rewritten when its stable key changes, so a
//! screen reader isn't disturbed by a ping or a byte counter moving.

use std::fmt::Write;
use std::time::Duration;

use crate::engine::{Health, PeerSnapshot, Snapshot};
use crate::profile::{Codec, Profile};
use crate::service::Firewall;
use crate::protocol::packet::{AudioFormat, CODEC_OPUS};

pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// "13 minutes 49 seconds", "2 hours 5 minutes", "8 seconds".
pub fn duration_long(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    match (hours, minutes) {
        (0, 0) => plural(seconds as usize, "second", "seconds"),
        (0, _) => format!("{} {}", plural(minutes as usize, "minute", "minutes"), plural(seconds as usize, "second", "seconds")),
        _ => format!("{} {}", plural(hours as usize, "hour", "hours"), plural(minutes as usize, "minute", "minutes")),
    }
}

/// "12 minutes", "less than a minute", "3 hours 2 minutes": for things measured in minutes.
pub fn duration_short(duration: Duration) -> String {
    let minutes = duration.as_secs() / 60;
    match minutes {
        0 => "less than a minute".to_string(),
        m if m < 60 => plural(m as usize, "minute", "minutes"),
        m => format!("{} {}", plural((m / 60) as usize, "hour", "hours"), plural((m % 60) as usize, "minute", "minutes")),
    }
}

pub fn kilobytes(bytes_per_second: f64) -> String {
    format!("{:.1} kB/s", bytes_per_second / 1000.0)
}

pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

fn health_word(health: Health) -> &'static str {
    match health {
        Health::Connecting => "connecting",
        Health::Healthy => "healthy",
        Health::Stale => "unsteady",
        Health::Unreachable => "not answering",
    }
}

fn format_description(format: &AudioFormat) -> String {
    let codec = if format.codec == CODEC_OPUS { "Opus" } else { "PCM 24 bit" };
    let ms = format.frame_samples as f32 * 1000.0 / format.sample_rate.max(1) as f32;
    format!("{codec} at {} Hz, {ms:.1} ms packets", format.sample_rate)
}

/// What we put on the wire for every ticked peer.
pub fn sending_description(profile: &Profile, snapshot: &Snapshot) -> String {
    if !profile.send_enabled {
        return "nothing (Send my audio is off)".into();
    }
    if profile.password.is_empty() {
        return "nothing yet (set a password first)".into();
    }
    if profile.sources.is_empty() {
        return "nothing (no source is ticked on the Audio tab)".into();
    }
    if snapshot.sources_running == 0 {
        return "nothing (the ticked sources could not be opened)".into();
    }
    format!(
        "{} on WASAPI at 48000 Hz, {}",
        plural(snapshot.sources_running, "device", "devices"),
        profile.codec.short_name()
    )
}

/// Far-end capture, our output device and one packet of the incoming stream.
pub fn hardware_ms(snapshot: &Snapshot) -> f32 {
    let output = if snapshot.output_latency_ms > 0.0 { snapshot.output_latency_ms } else { 20.0 };
    let incoming = snapshot.peers.iter().filter_map(|p| p.their_format.as_ref());
    let capture = incoming.clone().map(|f| f.capture_latency_ms).filter(|ms| *ms > 0.0).fold(0.0, f32::max);
    let packet = incoming
        .map(|f| f.frame_samples as f32 * 1000.0 / f.sample_rate.max(1) as f32)
        .fold(0.0, f32::max);
    output + if capture > 0.0 { capture } else { 10.0 } + if packet > 0.0 { packet } else { 10.0 }
}

fn peer_total_ms(peer: &PeerSnapshot, snapshot: &Snapshot) -> Option<f32> {
    let rtt = peer.rtt_ms? as f32;
    Some(rtt / 2.0 + snapshot.jitter_ms as f32 + hardware_ms(snapshot))
}

pub fn peer_details(peer: &PeerSnapshot, profile: &Profile, snapshot: &Snapshot) -> (String, String) {
    let mut text = String::new();
    let _ = writeln!(text, "Machine name: {}", peer.name);
    let _ = writeln!(text, "Address: {}", peer.address);
    match peer.connected_for {
        Some(d) => {
            let _ = writeln!(text, "Connected for: {}", duration_short(d));
        }
        None => {
            let _ = writeln!(text, "Connected for: not connected");
        }
    }
    match (peer.health, peer.rtt_ms) {
        (Health::Healthy | Health::Stale, Some(rtt)) => {
            let _ = writeln!(text, "Link: {}, ping {rtt} ms", health_word(peer.health));
        }
        (health, _) => {
            let _ = writeln!(text, "Link: {}", health_word(health));
        }
    }
    let sending = sending_description(profile, snapshot);
    let _ = writeln!(text, "Sending: {sending}");
    let receiving = match peer.can_receive {
        Some(true) => "yes",
        Some(false) => "no, its Receive audio is off",
        None => "unknown, it isn't announcing itself on the network",
    };
    let _ = writeln!(text, "Receiving your audio: {receiving}");
    let theirs = if peer.password_mismatch {
        "no, its password differs from yours".to_string()
    } else if let Some(format) = &peer.their_format {
        let description = format_description(format);
        if !profile.receive_enabled {
            format!("{description}, not played (Receive audio is off)")
        } else if peer.receiving_their_audio {
            format!("{description}, playing")
        } else {
            format!("{description}, but no audio is arriving")
        }
    } else {
        "nothing".to_string()
    };
    let _ = write!(text, "Their audio: {theirs}");
    let key = format!(
        "{}|{}|{}|{}|{}|{}|{}",
        peer.name,
        peer.address,
        health_word(peer.health),
        sending,
        receiving,
        theirs,
        peer.connected_for.map(|d| d.as_secs() / 60).unwrap_or(u64::MAX)
    );
    (text, key)
}

pub fn candidate_details(name: &str, address: std::net::Ipv4Addr, offers: Option<(bool, bool)>, wants: bool) -> (String, String) {
    let offers = match offers {
        Some((true, true)) => "On the network, sending and receiving audio.",
        Some((true, false)) => "On the network, sending audio only.",
        Some((false, true)) => "On the network, receiving audio only.",
        Some((false, false)) => "On the network, neither sending nor receiving audio.",
        None => "Not announcing itself on the network.",
    };
    let request = if wants {
        "It has connected to you with your password and is waiting to be accepted. Tick it to accept."
    } else {
        "Not connected. Tick it to move it to your connected peers."
    };
    let text = format!("Machine name: {name}\nAddress: {address}\n{offers}\n{request}");
    (text.clone(), text)
}

pub struct Rates {
    pub received_per_second: f64,
    pub sent_per_second: f64,
}

pub fn connection_status(
    profile: &Profile,
    snapshot: &Snapshot,
    rates: &Rates,
    cpu_percent: f64,
    memory_bytes: u64,
    firewall: Firewall,
    update: Option<&str>,
) -> (String, String) {
    let mut text = String::new();
    let mut key = String::new();
    let connected: Vec<&PeerSnapshot> =
        snapshot.peers.iter().filter(|p| matches!(p.health, Health::Healthy | Health::Stale)).collect();
    if snapshot.peers.is_empty() {
        text.push_str("No peers ticked. Tick a discovered peer, or add one by address or name.\n");
    } else if connected.is_empty() {
        let _ = writeln!(
            text,
            "Not connected. {} ticked, none answering yet.",
            plural(snapshot.peers.len(), "peer", "peers")
        );
    } else {
        let _ = writeln!(text, "Connected to {}.", plural(connected.len(), "peer", "peers"));
    }
    for peer in &snapshot.peers {
        match (peer.health, peer_total_ms(peer, snapshot)) {
            _ if peer.password_mismatch => {
                let _ = writeln!(text, "  {}: password differs from yours", peer.name);
            }
            (Health::Healthy | Health::Stale, Some(total)) => {
                let _ = writeln!(
                    text,
                    "  {}: ping {} ms, total latency about {:.0} ms",
                    peer.name,
                    peer.rtt_ms.unwrap_or(0),
                    total
                );
            }
            (health, _) => {
                let _ = writeln!(text, "  {}: {}", peer.name, health_word(health));
            }
        }
        let _ = write!(key, "{}:{}:{};", peer.name, health_word(peer.health), peer.password_mismatch);
    }
    let _ = writeln!(text, "Uptime: {}.", duration_long(snapshot.uptime));
    let _ = writeln!(
        text,
        "Receiving {}; sending {}.",
        kilobytes(rates.received_per_second),
        kilobytes(rates.sent_per_second)
    );
    let _ = writeln!(
        text,
        "Total received {}; sent {}.",
        megabytes(snapshot.bytes_received),
        megabytes(snapshot.bytes_sent)
    );
    let _ = write!(text, "CPU usage {:.0}% and memory {}.", cpu_percent, megabytes(memory_bytes));

    let mut problems = Vec::new();
    if profile.password.is_empty() {
        problems.push("Set a password on this tab: audio is always encrypted, so nothing is sent or played without one.".to_string());
    } else if !snapshot.keys_ready {
        problems.push("Preparing the password\u{2026}".to_string());
    }
    match firewall {
        Firewall::Blocked => problems.push(
            "Windows Firewall blocks RemSoundLite, so other computers can't reach it and no peers are discovered. \
             Use File, Allow through Windows Firewall."
                .to_string(),
        ),
        Firewall::NoRule => problems.push(
            "Windows Firewall has no rule for RemSoundLite yet. If peers can't reach it, use File, Allow through \
             Windows Firewall."
                .to_string(),
        ),
        Firewall::Allowed | Firewall::Unknown => {}
    }
    if let Some(update) = update {
        problems.push(format!("{update} Use File, Check for updates to install it."));
    }
    if profile.receive_enabled && profile.outputs.is_empty() {
        problems.push("Receive audio is on, but no output is ticked on the Audio tab.".to_string());
    }
    if let Some(e) = &snapshot.port_error {
        problems.push(format!("Not listening: {e}. Is RemSound or another copy running?"));
    }
    if let Some(e) = &snapshot.discovery_error {
        problems.push(format!("Peer discovery is off: {e}."));
    }
    for e in &snapshot.source_errors {
        problems.push(format!("A source could not be opened: {e}."));
    }
    for e in &snapshot.output_errors {
        problems.push(format!("An output could not be opened: {e}."));
    }
    for problem in &problems {
        let _ = write!(text, "\n{problem}");
        key.push_str(problem);
    }
    let _ = write!(key, "|{}", connected.len());
    (text, key)
}

pub fn total_latency(profile: &Profile, snapshot: &Snapshot) -> (String, String) {
    let hardware = hardware_ms(snapshot).round();
    let jitter = snapshot.jitter_ms;
    let buffer = if profile.auto_tune && jitter != profile.jitter_ms {
        format!("Jitter buffer {jitter} ms (auto-tuned from {} ms).", profile.jitter_ms)
    } else {
        format!("Jitter buffer {jitter} ms.")
    };
    let text = format!(
        "{buffer} Sound card and hardware add {hardware:.0} ms. Total latency approximately {:.0} ms one way.",
        jitter as f32 + hardware
    );
    (text.clone(), text)
}

pub fn codec_index(codec: Codec) -> u32 {
    Codec::ALL.iter().position(|c| *c == codec).unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_long(Duration::from_secs(13 * 60 + 49)), "13 minutes 49 seconds");
        assert_eq!(duration_long(Duration::from_secs(1)), "1 second");
        assert_eq!(duration_long(Duration::from_secs(3600 * 2 + 60)), "2 hours 1 minute");
        assert_eq!(duration_short(Duration::from_secs(12 * 60 + 5)), "12 minutes");
        assert_eq!(duration_short(Duration::from_secs(30)), "less than a minute");
    }

    #[test]
    fn units() {
        assert_eq!(kilobytes(25_400.0), "25.4 kB/s");
        assert_eq!(megabytes(20_800_000), "20.8 MB");
    }

    #[test]
    fn status_mentions_a_missing_password() {
        let profile = Profile::default();
        let snapshot = Snapshot::default();
        let rates = Rates { received_per_second: 0.0, sent_per_second: 0.0 };
        let (text, _) = connection_status(&profile, &snapshot, &rates, 1.0, 144_400_000, Firewall::Blocked, None);
        assert!(text.contains("Set a password"));
        assert!(text.contains("CPU usage 1% and memory 144.4 MB."));
        assert!(text.contains("Allow through Windows Firewall"));
    }
}
