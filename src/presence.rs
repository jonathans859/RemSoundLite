//! "The window is open": a machine-wide named mutex the GUI holds for as long as it runs.
//! The lock-screen service polls it and hands over (stops sending, frees the UDP port)
//! whenever the GUI is up. It also keeps a second copy of the GUI from starting.

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};
use windows::core::w;

/// Held by the GUI thread; dropping it (or the process ending) releases the mutex.
pub struct PresenceHold(HANDLE);

impl Drop for PresenceHold {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// `None` when another copy of the window already holds it.
pub fn acquire() -> Option<PresenceHold> {
    unsafe {
        let handle = CreateMutexW(None, true, w!("Global\\RemSoundLite.Interactive")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // It existed already, so we were not made its owner. Take it if nobody holds it
            // (the service may just have had it open to look).
            let wait = WaitForSingleObject(handle, 0);
            if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
                let _ = CloseHandle(handle);
                return None;
            }
        }
        Some(PresenceHold(handle))
    }
}

/// True while a GUI copy holds the mutex. Used by the service.
pub fn interactive_app_running() -> bool {
    unsafe {
        let Ok(handle) = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Global\\RemSoundLite.Interactive")) else {
            return false;
        };
        let wait = WaitForSingleObject(handle, 0);
        let running = wait == WAIT_TIMEOUT;
        if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
            let _ = ReleaseMutex(handle);
        }
        let _ = CloseHandle(handle);
        running
    }
}
