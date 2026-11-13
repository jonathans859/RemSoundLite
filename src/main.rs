#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! RemSoundLite: an accessible Windows client for the RemSound audio protocol.
//!
//! One executable, several roles:
//! * no arguments: the window (and tray icon);
//! * `--service`: the lock-screen service, as started by the service manager;
//! * `--install-service <profile.json>` / `--uninstall-service`: elevated helpers the window
//!   runs through UAC, and `--allow-firewall` for the Windows Firewall rule.

mod audio;
mod engine;
mod gui;
mod presence;
mod profile;
mod protocol;
mod service;
mod speech;
mod sysinfo;
mod updates;

use std::path::Path;

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    match arguments.get(1).map(String::as_str) {
        Some("--service") => {
            if let Err(e) = service::run() {
                eprintln!("This is the RemSoundLite service entry point; Windows starts it. ({e})");
                std::process::exit(1);
            }
        }
        Some("--install-service") => {
            let result = match arguments.get(2) {
                Some(path) => service::install(Path::new(path)),
                None => Err("--install-service needs the path of a profile file".into()),
            };
            std::process::exit(service::report(&result));
        }
        Some("--uninstall-service") => std::process::exit(service::report(&service::uninstall())),
        Some("--allow-firewall") => std::process::exit(service::report(&service::allow_firewall())),
        _ => gui::run(),
    }
}
