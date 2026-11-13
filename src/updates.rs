//! Updates from GitHub Releases with ship-shape.
//!
//! Each release carries `RemSoundLite.zip` (the exe) and `RemSoundLite.zip.minisig`, signed in
//! CI with the key whose public half is below (see `tools/minisign.py` and
//! `.github/workflows/release.yml`). A download that doesn't verify against it is refused.
//!
//! Two channels, chosen in Settings, Updates:
//! * Stable: versioned releases from `.github/workflows/release.yml` (tags such as v0.2.0).
//! * Development: the rolling pre-release tagged `latest`, rebuilt by `.github/workflows/dev.yml`
//!   on every push to main. Its notes list commits newest first; each build knows its own
//!   commit (embedded by `build.rs`), so it's offered only what came after it. GitHub never
//!   picks a pre-release as the latest release, so the stable channel doesn't see it.
//!
//! * At startup a silent check runs on a thread after a short wait. It never opens a dialog
//!   (the window may be hidden in the tray, where a dialog would be lost); it only reports.
//! * File, Check for updates runs ship-shape's own accessible flow. When the new version is
//!   ready, ship-shape calls our exit handler from its thread, and the GUI timer quits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use ship_shape::ui::CheckTrigger;
use ship_shape::{InstallKind, UpdateChannel, UpdateCheckOutcome, UpdaterConfig};

use crate::profile;
use wxdragon::prelude::WxWidget;

pub const GITHUB_REPO: &str = "jonathans859/RemSoundLite";
/// The minisign public key releases are signed with.
pub const PUBLIC_KEY: &str = "RWStiZa9hoO3tCTO4AcLe713AlJtpVAfyahBYo7BjRLhUdckoFCGQNpU";

/// The commit this exe was built from, or empty when built outside a git checkout.
pub const COMMIT: &str = env!("REMSOUNDLITE_COMMIT");

pub fn config() -> UpdaterConfig {
    UpdaterConfig::new(GITHUB_REPO, "RemSoundLite", "RemSoundLite", PUBLIC_KEY, env!("CARGO_PKG_VERSION"))
        .with_commit(COMMIT)
        .with_install_kind(InstallKind::Portable)
}

fn channel_of(channel: profile::UpdateChannel) -> UpdateChannel {
    match channel {
        profile::UpdateChannel::Stable => UpdateChannel::Stable,
        profile::UpdateChannel::Development => UpdateChannel::Dev,
    }
}

/// "version 0.1.0 (build 1a2b3c4)", for speech after an update.
pub fn build_name() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match COMMIT.get(..7) {
        Some(short) => format!("version {version} (build {short})"),
        None => format!("version {version}"),
    }
}

/// Checks quietly after startup. Sends one line when a newer version exists, nothing otherwise.
pub fn check_in_background(channel: profile::UpdateChannel) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Give the network a moment at sign-in.
        std::thread::sleep(Duration::from_secs(10));
        if let Ok(UpdateCheckOutcome::UpdateAvailable(update)) =
            ship_shape::check_for_updates(&config(), channel_of(channel))
        {
            let notice = match channel {
                profile::UpdateChannel::Stable => {
                    let version = update.latest_version.trim_start_matches('v');
                    format!("Version {version} of RemSoundLite is available.")
                }
                profile::UpdateChannel::Development => "A new development build of RemSoundLite is available.".into(),
            };
            let _ = tx.send(notice);
        }
    });
    rx
}

/// The user asked: show "up to date", errors, or the update dialog and download.
pub fn check_now(parent: &dyn WxWidget, exit: Arc<AtomicBool>, channel: profile::UpdateChannel) {
    ship_shape::ui::run_update_check_with_exit_handler(
        Arc::new(config()),
        parent,
        channel_of(channel),
        CheckTrigger::Manual,
        move || exit.store(true, Ordering::Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_key_parses() {
        assert!(minisign_verify::PublicKey::from_base64(PUBLIC_KEY).is_ok());
    }

    /// CI signs the release archive, then runs this against it with the public key compiled
    /// into the app, so a release the shipped updater would reject is never published:
    /// `RELEASE_ZIP=RemSoundLite.zip cargo test release_signature -- --ignored`
    #[test]
    #[ignore]
    fn release_signature_verifies() {
        let path = std::env::var("RELEASE_ZIP").expect("RELEASE_ZIP");
        let data = std::fs::read(&path).unwrap();
        let signature = std::fs::read_to_string(format!("{path}.minisig")).unwrap();
        let key = minisign_verify::PublicKey::from_base64(PUBLIC_KEY).unwrap();
        let signature = minisign_verify::Signature::decode(&signature).unwrap();
        key.verify(&data, &signature, false).expect("the release must verify with the app's key");
    }
}
