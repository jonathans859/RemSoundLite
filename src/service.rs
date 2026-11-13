//! The lock-screen service: a send-only copy of the engine that Windows starts at boot, so
//! audio (a screen reader on the sign-in screen, for instance) reaches your peers before
//! anyone has logged in.
//!
//! * Runs as LocalSystem, started automatically, after the Windows Audio service.
//! * Reads `%ProgramData%\RemSoundLite\service-profile.json`, a copy of one of your profiles
//!   made when the service is installed (readable only by SYSTEM and administrators, since
//!   it holds the password), and re-reads it when it changes.
//! * Steps aside while the window is open (see `presence`), so the two never fight over the
//!   UDP port, and takes over again a couple of seconds after the window closes.
//!
//! The same executable does everything: `--service` is what the service manager runs,
//! `--install-service <profile.json>` and `--uninstall-service` need an elevated process,
//! which the window starts through UAC.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceDependency, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::engine::{Engine, EngineConfig};
use crate::presence;
use crate::profile::{Profile, service_profile_path};

pub const SERVICE_NAME: &str = "RemSoundLite";
const DISPLAY_NAME: &str = "RemSoundLite lock-screen sender";
const DESCRIPTION: &str = "Sends this computer's audio to your RemSound peers from startup, including on the lock \
screen. Steps aside while the RemSoundLite window is open.";

fn data_dir() -> PathBuf {
    service_profile_path().parent().map(Path::to_path_buf).unwrap_or_default()
}

pub fn install_log_path() -> PathBuf {
    data_dir().join("service-install.log")
}

fn log(message: &str) {
    let path = data_dir().join("service.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(file, "{stamp} {message}");
    }
}

// ============================================================== running as a service

define_windows_service!(ffi_service_main, service_main);

/// Called from `main` for `--service`. Blocks until the service stops.
pub fn run() -> Result<(), String> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.to_string())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        log(&format!("service failed: {e}"));
    }
}

fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    }
}

fn run_service() -> Result<(), String> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler).map_err(|e| e.to_string())?;
    status_handle.set_service_status(status(ServiceState::Running)).map_err(|e| e.to_string())?;
    log("service started");

    let path = service_profile_path();
    let mut engine: Option<Engine> = None;
    let mut loaded_at: Option<SystemTime> = None;
    let mut handed_over = false;
    loop {
        let window_open = presence::interactive_app_running();
        if window_open {
            if engine.take().is_some() || !handed_over {
                log("the window is open: handing over to it");
            }
            handed_over = true;
            loaded_at = None;
        } else {
            handed_over = false;
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
            if engine.is_none() || modified != loaded_at {
                match Profile::load(&path) {
                    Ok(profile) => {
                        let config = EngineConfig::from_profile(&profile, true);
                        log(&format!(
                            "sending to {} peer(s) from {} source(s)",
                            config.peers.len(),
                            config.sources.len()
                        ));
                        match engine.as_ref() {
                            Some(running) => running.apply(config),
                            None => engine = Some(Engine::start(config)),
                        }
                    }
                    Err(e) => {
                        if loaded_at.is_some() || engine.is_none() {
                            log(&format!("no usable profile: {e}"));
                        }
                        engine = None;
                    }
                }
                loaded_at = modified;
            }
        }
        if stop_rx.recv_timeout(Duration::from_secs(2)).is_ok() {
            break;
        }
    }
    drop(engine);
    log("service stopped");
    status_handle.set_service_status(status(ServiceState::Stopped)).map_err(|e| e.to_string())?;
    Ok(())
}

// ============================================================== install / uninstall (elevated)

fn restrict_to_admins(path: &Path) -> Result<(), String> {
    // SYSTEM (S-1-5-18) and Administrators (S-1-5-32-544) only: the file holds the password.
    let output = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:F", "*S-1-5-32-544:F"])
        .output()
        .map_err(|e| format!("icacls: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("icacls failed: {}", String::from_utf8_lossy(&output.stdout).trim()))
    }
}

// ============================================================== Windows Firewall

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firewall {
    Allowed,
    /// A rule blocks the program: Windows makes one when its "allow access" prompt is
    /// dismissed, and then peers can't reach us at all.
    Blocked,
    /// No rule either way yet.
    NoRule,
    Unknown,
}

/// Reads the inbound rules for this executable. Takes a second or two (PowerShell's
/// NetSecurity module, which reports actions in English whatever the Windows language), so
/// call it off the GUI thread. Needs no elevation.
pub fn firewall_state() -> Firewall {
    let Ok(executable) = std::env::current_exe() else { return Firewall::Unknown };
    let path = executable.display().to_string().replace('\'', "''");
    let script = format!(
        "Get-NetFirewallApplicationFilter -Program '{path}' -ErrorAction SilentlyContinue | Get-NetFirewallRule | \
         Where-Object {{ $_.Enabled -eq 'True' -and $_.Direction -eq 'Inbound' }} | ForEach-Object {{ [string]$_.Action }}"
    );
    use std::os::windows::process::CommandExt;
    let Ok(output) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
    else {
        return Firewall::Unknown;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    if text.lines().any(|l| l.trim() == "Block") {
        Firewall::Blocked
    } else if text.lines().any(|l| l.trim() == "Allow") {
        Firewall::Allowed
    } else {
        Firewall::NoRule
    }
}

/// The service runs its own copy of the program, so updating the app (which replaces the exe
/// next to it) never trips over a file the service holds open.
fn service_executable() -> PathBuf {
    data_dir().join("bin").join("remsoundlite.exe")
}

fn allow_program(executable: &Path) -> Result<(), String> {
    let program = format!("program={}", executable.display());
    let netsh = |arguments: &[&str]| std::process::Command::new("netsh").args(arguments).output();
    // Fails harmlessly with "No rules match" when there are none.
    let _ = netsh(&["advfirewall", "firewall", "delete", "rule", "name=all", "dir=in", &program]);
    let output = netsh(&[
        "advfirewall", "firewall", "add", "rule", "name=RemSoundLite", "dir=in", "action=allow", &program,
        "enable=yes", "profile=any",
    ])
    .map_err(|e| format!("netsh: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("Windows Firewall refused the rule: {}", String::from_utf8_lossy(&output.stdout).trim()))
    }
}

/// Elevated: removes every inbound rule for this executable (block rules beat allow rules)
/// and adds one allowing it on all network profiles. The service's copy gets the same.
pub fn allow_firewall() -> Result<(), String> {
    allow_program(&std::env::current_exe().map_err(|e| e.to_string())?)?;
    if service_executable().exists() {
        allow_program(&service_executable())?;
    }
    Ok(())
}

fn wait_until_stopped(service: &windows_service::service::Service) {
    for _ in 0..100 {
        if service.query_status().map(|s| s.current_state == ServiceState::Stopped).unwrap_or(true) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Copies the profile and this program for the service, then creates (or updates) and starts
/// it. Running it again after an app update brings the service up to date too.
pub fn install(profile_source: &Path) -> Result<(), String> {
    let profile = Profile::load(profile_source)?;
    let target = service_profile_path();
    fs::create_dir_all(data_dir().join("bin")).map_err(|e| e.to_string())?;
    profile.save(&target)?;
    restrict_to_admins(&target)?;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
        .map_err(|e| format!("could not open the service manager: {e}"))?;
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
    let existing = manager.open_service(SERVICE_NAME, access).ok();
    if let Some(service) = &existing {
        // Its exe can't be replaced while it runs.
        if service.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false) {
            let _ = service.stop();
            wait_until_stopped(service);
        }
    }
    let executable = service_executable();
    let source = std::env::current_exe().map_err(|e| e.to_string())?;
    if source != executable {
        fs::copy(&source, &executable).map_err(|e| format!("could not copy the program for the service: {e}"))?;
    }
    // Peers have to reach the service's copy as well as the window's.
    allow_firewall()?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![ServiceDependency::Service(OsString::from("Audiosrv"))],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = match existing {
        Some(service) => {
            service.change_config(&info).map_err(|e| format!("could not update the service: {e}"))?;
            service
        }
        None => manager.create_service(&info, access).map_err(|e| format!("could not create the service: {e}"))?,
    };
    let _ = service.set_description(DESCRIPTION);
    service.start::<&OsStr>(&[]).map_err(|e| format!("the service was installed but did not start: {e}"))?;
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| format!("could not open the service manager: {e}"))?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
        .map_err(|e| format!("the service is not installed ({e})"))?;
    if service.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false) {
        let _ = service.stop();
        wait_until_stopped(&service);
    }
    service.delete().map_err(|e| format!("could not remove the service: {e}"))?;
    let program = format!("program={}", service_executable().display());
    let _ = std::process::Command::new("netsh")
        .args(["advfirewall", "firewall", "delete", "rule", "name=all", "dir=in", &program])
        .output();
    let _ = fs::remove_file(service_profile_path());
    let _ = fs::remove_dir_all(data_dir().join("bin"));
    Ok(())
}

/// Whether the service is installed, for the menu. Needs no elevation.
pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

/// Runs this executable elevated (Windows asks through UAC) with `arguments`, waits for it and
/// returns its outcome. Blocks, so call it off the GUI thread.
pub fn run_elevated(arguments: &str) -> Result<(), String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    use windows::core::{HSTRING, PCWSTR, w};

    let executable = HSTRING::from(std::env::current_exe().map_err(|e| e.to_string())?.as_os_str());
    let parameters = HSTRING::from(arguments);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    let _ = fs::remove_file(install_log_path());
    unsafe {
        ShellExecuteExW(&mut info).map_err(|_| "Administrator permission was not given.".to_string())?;
        if info.hProcess.is_invalid() {
            return Err("The elevated helper did not start.".into());
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        if code == 0 {
            Ok(())
        } else {
            Err(fs::read_to_string(install_log_path()).unwrap_or_else(|_| format!("The helper failed (code {code}).")))
        }
    }
}

/// Writes the outcome of an elevated install or uninstall where the window can read it.
pub fn report(result: &Result<(), String>) -> i32 {
    let _ = fs::create_dir_all(data_dir());
    match result {
        Ok(()) => {
            let _ = fs::remove_file(install_log_path());
            0
        }
        Err(e) => {
            let _ = fs::write(install_log_path(), e);
            1
        }
    }
}
