//! Capturing one WASAPI endpoint into a queue: an input device (microphone, line in), or an
//! output device in loopback (what you hear on it). Always 48 kHz stereo float: shared mode
//! with automatic conversion lets Windows resample and remix for us.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

/// Samples (interleaved stereo) waiting to be mixed into the outgoing stream.
pub struct SourceQueue {
    pub samples: Mutex<VecDeque<f32>>,
    pub error: Mutex<Option<String>>,
    pub running: AtomicBool,
    /// Milliseconds of delay the device adds on its way in (its period).
    pub latency_ms: Mutex<f32>,
}

pub struct Capture {
    pub queue: Arc<SourceQueue>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// An output device's id captures what it plays (loopback); an input device's id
    /// captures the input itself.
    pub fn start(device_id: String) -> Self {
        let queue = Arc::new(SourceQueue {
            samples: Mutex::new(VecDeque::new()),
            error: Mutex::new(None),
            running: AtomicBool::new(false),
            latency_ms: Mutex::new(10.0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (q, s) = (queue.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                let _ = wasapi::initialize_mta();
                while !s.load(Ordering::Relaxed) {
                    if let Err(e) = run(&device_id, &q, &s) {
                        q.running.store(false, Ordering::Relaxed);
                        *q.error.lock().unwrap() = Some(e);
                        // Device unplugged or busy: try again shortly.
                        for _ in 0..10 {
                            if s.load(Ordering::Relaxed) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                q.running.store(false, Ordering::Relaxed);
            })
            .expect("spawning the capture thread");
        Self { queue, stop, thread: Some(thread) }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(device_id: &str, queue: &SourceQueue, stop: &AtomicBool) -> Result<(), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = match device_id {
        crate::profile::DEFAULT_INPUT_ID => enumerator.get_default_device(&Direction::Capture),
        crate::profile::DEFAULT_OUTPUT_ID => enumerator.get_default_device(&Direction::Render),
        id => enumerator.get_device(id),
    }
    .map_err(|e| format!("device not available: {e}"))?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    if let Ok((default_period, _)) = client.get_device_period() {
        *queue.latency_ms.lock().unwrap() = default_period as f32 / 10_000.0;
    }
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 1_000_000 };
    // Initializing a render device in the capture direction is what selects loopback.
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| format!("could not open the device: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    client.start_stream().map_err(|e| e.to_string())?;
    *queue.error.lock().unwrap() = None;
    queue.running.store(true, Ordering::Relaxed);

    let mut bytes = vec![0u8; 48_000 * 8];
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        // Loopback signals nothing while nothing plays, so a timeout is just "no data".
        if event.wait_for_event(100).is_err() {
            continue;
        }
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(frames)) => {
                    let needed = frames as usize * 8;
                    if bytes.len() < needed {
                        bytes.resize(needed, 0);
                    }
                }
                Err(e) => return Err(format!("capture stopped: {e}")),
            }
            let (frames, info) = match capture.read_from_device(&mut bytes) {
                Ok(read) => read,
                Err(e) => return Err(format!("capture stopped: {e}")),
            };
            let count = frames as usize * 2;
            let mut samples = queue.samples.lock().unwrap();
            if info.flags.silent {
                samples.extend(std::iter::repeat_n(0.0, count));
            } else {
                samples.extend(
                    bytes[..count * 4].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)),
                );
            }
            // Never let an unread source grow without bound (sending off, or clock drift).
            let cap = 48_000 * 2 / 5; // 200 ms
            if samples.len() > cap {
                let excess = samples.len() - cap;
                samples.drain(..excess);
            }
        }
    };
    let _ = client.stop_stream();
    result
}
