//! This process's CPU usage and memory, for the connection status.

use std::time::Instant;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

fn filetime_100ns(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}

/// Busy time of this process, all threads, in 100 ns units.
fn process_cpu_100ns() -> u64 {
    let (mut creation, mut exit, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user).is_err() {
            return 0;
        }
    }
    filetime_100ns(kernel) + filetime_100ns(user)
}

/// Working set in bytes.
pub fn memory_bytes() -> u64 {
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        if !K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).as_bool() {
            return 0;
        }
    }
    counters.WorkingSetSize as u64
}

/// CPU usage as a share of the whole machine, measured between calls.
pub struct CpuMeter {
    last_wall: Instant,
    last_cpu: u64,
    cpus: f64,
    pub percent: f64,
}

impl CpuMeter {
    pub fn new() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu: process_cpu_100ns(),
            cpus: std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(1.0),
            percent: 0.0,
        }
    }

    pub fn update(&mut self) {
        let now = Instant::now();
        let cpu = process_cpu_100ns();
        let wall_100ns = (now - self.last_wall).as_nanos() as f64 / 100.0;
        if wall_100ns > 0.0 {
            self.percent = (cpu.saturating_sub(self.last_cpu) as f64 / wall_100ns / self.cpus * 100.0).clamp(0.0, 100.0);
        }
        self.last_wall = now;
        self.last_cpu = cpu;
    }
}
