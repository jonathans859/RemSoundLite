//! Profiles: every setting of the window, the password included, saved as one JSON file per
//! profile in `%APPDATA%\RemSoundLite\profiles`. The lock-screen service reads its own copy
//! from `%ProgramData%\RemSoundLite\service-profile.json`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::jitter::Artefact;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    #[default]
    OpusLive,
    OpusBroadcast,
    Pcm24,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::OpusLive, Codec::OpusBroadcast, Codec::Pcm24];

    pub fn label(self) -> &'static str {
        match self {
            Codec::OpusLive => "Opus, live latency \u{2014} for jamming and monitoring",
            Codec::OpusBroadcast => "Opus, broadcast quality \u{2014} loss tolerant",
            Codec::Pcm24 => "PCM 48K 24 bit \u{2014} uncompressed",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Codec::OpusLive | Codec::OpusBroadcast => "Opus",
            Codec::Pcm24 => "PCM 24 bit",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PacketSize {
    #[default]
    Standard,
    Small,
}

impl PacketSize {
    pub const ALL: [PacketSize; 2] = [PacketSize::Standard, PacketSize::Small];

    pub fn label(self) -> &'static str {
        match self {
            PacketSize::Standard => "Standard (5 ms PCM, 10/20 ms Opus)",
            PacketSize::Small => "Small (2.5 ms PCM, 5/10 ms Opus, LAN only)",
        }
    }
}

/// Samples per channel at 48 kHz in one packet for a codec and packet size. A standard PCM
/// frame is 233 samples (just under 5 ms), the largest whose ciphertext still fits one packet.
pub fn frame_samples(codec: Codec, size: PacketSize) -> i32 {
    match (codec, size) {
        (Codec::OpusLive, PacketSize::Standard) => 480,
        (Codec::OpusLive, PacketSize::Small) => 240,
        (Codec::OpusBroadcast, PacketSize::Standard) => 960,
        (Codec::OpusBroadcast, PacketSize::Small) => 480,
        (Codec::Pcm24, PacketSize::Standard) => 233,
        (Codec::Pcm24, PacketSize::Small) => 120,
    }
}

pub const ARTEFACTS: [(Artefact, &str); 2] = [
    (Artefact::NoiseBurst, "Noise burst (default) \u{2014} broadband shhh, blends into music"),
    (Artefact::Click, "Click \u{2014} no concealment, raw zero-fill click"),
];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPeer {
    pub name: String,
    pub address: String,
    /// Ticked: we send to it, ping it and accept its audio.
    pub active: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDevice {
    pub id: String,
    pub name: String,
}

/// Device ids that follow whatever Windows uses as its default at the time.
pub const DEFAULT_INPUT_ID: &str = "default:input";
pub const DEFAULT_OUTPUT_ID: &str = "default:output";

impl SavedDevice {
    pub fn default_input() -> Self {
        Self { id: DEFAULT_INPUT_ID.into(), name: "Default input".into() }
    }

    pub fn default_output() -> Self {
        Self { id: DEFAULT_OUTPUT_ID.into(), name: "Default output".into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub password: String,
    pub peers: Vec<SavedPeer>,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    /// Ticked sources: input devices, and output devices captured in loopback.
    pub sources: Vec<SavedDevice>,
    /// Ticked outputs received audio plays on.
    pub outputs: Vec<SavedDevice>,
    /// Volume of received audio, 0-100 percent.
    pub volume: u8,
    /// Peers that tick you (and prove they hold the password) are added and ticked at once,
    /// rather than waiting in the discovered list for you to tick them.
    pub auto_accept: bool,
    pub codec: Codec,
    pub packet_size: PacketSize,
    pub jitter_ms: u32,
    pub auto_tune: bool,
    pub smoothness: u8,
    pub artefact: Artefact,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            password: String::new(),
            peers: Vec::new(),
            send_enabled: true,
            receive_enabled: true,
            sources: Vec::new(),
            outputs: vec![SavedDevice::default_output()],
            volume: 100,
            auto_accept: false,
            codec: Codec::OpusLive,
            packet_size: PacketSize::Standard,
            jitter_ms: 80,
            auto_tune: false,
            smoothness: 3,
            artefact: Artefact::NoiseBurst,
        }
    }
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let raw: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{} is not a valid profile: {e}", path.display()))?;
        // Profiles from before the volume setting took "no output ticked" to mean the default output.
        let from_before_defaults = raw.get("volume").is_none();
        let mut profile: Profile =
            serde_json::from_value(raw).map_err(|e| format!("{} is not a valid profile: {e}", path.display()))?;
        if from_before_defaults && profile.outputs.is_empty() {
            profile.outputs.push(SavedDevice::default_output());
        }
        profile.jitter_ms = profile.jitter_ms.clamp(MIN_JITTER_MS, MAX_JITTER_MS);
        profile.smoothness = profile.smoothness.clamp(1, 10);
        profile.volume = profile.volume.min(100);
        Ok(profile)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Write beside the file and rename, so a crash never leaves half a profile.
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text).map_err(|e| format!("{}: {e}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

pub const MIN_JITTER_MS: u32 = 10;
pub const MAX_JITTER_MS: u32 = 500;
pub const DEFAULT_PROFILE: &str = "Default";

/// `%APPDATA%\RemSoundLite`, or `REMSOUNDLITE_CONFIG_DIR` when set: a separate set of profiles
/// for testing, or a portable copy that keeps its settings beside it.
fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("REMSOUNDLITE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("RemSoundLite")
}

pub fn service_profile_path() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("RemSoundLite").join("service-profile.json")
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Settings {
    last_profile: String,
}

/// The user's profiles, one file each, named after the profile.
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self { dir: app_data_dir().join("profiles") }
    }

    pub fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if path.extension()? != "json" {
                    return None;
                }
                Some(path.file_stem()?.to_string_lossy().into_owned())
            })
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    pub fn load(&self, name: &str) -> Result<Profile, String> {
        Profile::load(&self.path_of(name))
    }

    pub fn save(&self, name: &str, profile: &Profile) -> Result<(), String> {
        profile.save(&self.path_of(name))
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        fs::remove_file(self.path_of(name)).map_err(|e| e.to_string())
    }

    fn settings_path(&self) -> PathBuf {
        app_data_dir().join("settings.json")
    }

    pub fn last_used(&self) -> Option<String> {
        let text = fs::read_to_string(self.settings_path()).ok()?;
        let settings: Settings = serde_json::from_str(&text).ok()?;
        (!settings.last_profile.is_empty()).then_some(settings.last_profile)
    }

    pub fn set_last_used(&self, name: &str) {
        let settings = Settings { last_profile: name.to_string() };
        if let Ok(text) = serde_json::to_string_pretty(&settings) {
            let _ = fs::create_dir_all(app_data_dir());
            let _ = fs::write(self.settings_path(), text);
        }
    }
}

/// A profile name must work as a file name.
pub fn valid_profile_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 80
        && !name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control())
        && !name.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trips_and_fills_missing_fields() {
        let dir = std::env::temp_dir().join(format!("remsoundlite-test-{}", std::process::id()));
        let path = dir.join("p.json");
        let mut profile = Profile { password: "secret".into(), jitter_ms: 120, ..Default::default() };
        profile.peers.push(SavedPeer { name: "hermes".into(), address: "100.110.55.121".into(), active: true });
        profile.save(&path).unwrap();
        assert_eq!(Profile::load(&path).unwrap(), profile);
        std::fs::write(&path, r#"{"password":"x","jitter_ms":5}"#).unwrap();
        let partial = Profile::load(&path).unwrap();
        assert_eq!(partial.password, "x");
        assert_eq!(partial.jitter_ms, MIN_JITTER_MS);
        assert_eq!(partial.smoothness, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_that_cannot_be_files_are_refused() {
        assert!(valid_profile_name("Studio"));
        assert!(!valid_profile_name(" "));
        assert!(!valid_profile_name("a/b"));
        assert!(!valid_profile_name("end."));
    }
}
