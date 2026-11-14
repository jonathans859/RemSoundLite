//! Starting with Windows: a value under the user's `Run` key, so the window opens (or, with
//! "Start hidden", only the notification-area icon) when this user logs in. No elevation needed.
//!
//! Windows is the record, not `settings.json`: the user can also switch the entry off in
//! Settings, Apps, Startup or Task Manager, which writes a "disabled" flag under
//! `StartupApproved\Run` and leaves the `Run` value alone. So "enabled" means the value is
//! there and not switched off, and turning it on clears that flag.

use std::path::Path;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::w;

const RUN: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const APPROVED: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run");
const VALUE: windows::core::PCWSTR = w!("RemSoundLite");

/// Whether RemSoundLite starts when this user logs in.
pub fn is_enabled() -> bool {
    run_value_exists() && !switched_off()
}

/// Starts this executable at login, or stops doing so.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        set_run_value(&exe)?;
        // Clears a "disabled" left by the Startup apps switch; harmless if there isn't one.
        let _ = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, APPROVED, VALUE) };
        Ok(())
    } else {
        check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, VALUE) })
    }
}

fn run_value_exists() -> bool {
    let status = unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, VALUE, RRF_RT_REG_SZ, None, None, None) };
    status == ERROR_SUCCESS
}

/// The Startup apps switch: 12 bytes whose first is even (2, 6) when on and odd (3, 7) when off.
fn switched_off() -> bool {
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            APPROVED,
            VALUE,
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && size > 0 && data[0] & 1 == 1
}

fn set_run_value(exe: &Path) -> Result<(), String> {
    // Quoted, since the path may have spaces; NUL-terminated, as REG_SZ wants.
    let command: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
    let bytes = (command.len() * 2) as u32;
    check(unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, RUN, VALUE, REG_SZ.0, Some(command.as_ptr().cast()), bytes) })
}

/// Removing a value that isn't there counts as done.
fn check(status: WIN32_ERROR) -> Result<(), String> {
    match status {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
        other => Err(windows::core::Error::from(other.to_hresult()).message()),
    }
}
