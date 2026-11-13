//! Playing received audio on one WASAPI output device. Every incoming stream has its own
//! playout buffer per output; the render thread mixes them on each device period.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use super::jitter::{BufferParams, PlayoutBuffer};

/// One incoming stream: who sent it and which stream id it carried.
pub type StreamKey = (Ipv4Addr, u16);

pub struct StreamSlot {
    pub buffer: PlayoutBuffer,
    pub last_write: Instant,
}

pub struct OutputMix {
    pub streams: HashMap<StreamKey, StreamSlot>,
    pub params: BufferParams,
    /// Gain applied to the mix, 0.0-1.0.
    pub volume: f32,
}

impl OutputMix {
    pub fn write(&mut self, key: StreamKey, samples: &[f32]) {
        let params = self.params;
        let slot = self.streams.entry(key).or_insert_with(|| StreamSlot {
            buffer: PlayoutBuffer::new(params),
            last_write: Instant::now(),
        });
        slot.buffer.write(samples);
        slot.last_write = Instant::now();
    }

    pub fn set_params(&mut self, params: BufferParams) {
        self.params = params;
        for slot in self.streams.values_mut() {
            slot.buffer.set_params(params);
        }
    }
}

pub struct Output {
    pub mix: Arc<Mutex<OutputMix>>,
    pub error: Arc<Mutex<Option<String>>>,
    pub running: Arc<AtomicBool>,
    /// The device's own share of the delay: its shared-mode buffer.
    pub latency_ms: Arc<Mutex<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    /// `device_id` of `None` plays on the current default output.
    pub fn start(device_id: Option<String>, params: BufferParams) -> Self {
        let mix = Arc::new(Mutex::new(OutputMix { streams: HashMap::new(), params, volume: 1.0 }));
        let error = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(false));
        let latency_ms = Arc::new(Mutex::new(20.0));
        let stop = Arc::new(AtomicBool::new(false));
        let (m, e, r, l, s) = (mix.clone(), error.clone(), running.clone(), latency_ms.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let _ = wasapi::initialize_mta();
                while !s.load(Ordering::Relaxed) {
                    if let Err(message) = run(device_id.as_deref(), &m, &r, &l, &s) {
                        r.store(false, Ordering::Relaxed);
                        *e.lock().unwrap() = Some(message);
                        for _ in 0..10 {
                            if s.load(Ordering::Relaxed) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                r.store(false, Ordering::Relaxed);
            })
            .expect("spawning the render thread");
        Self { mix, error, running, latency_ms, stop, thread: Some(thread) }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    device_id: Option<&str>,
    mix: &Mutex<OutputMix>,
    running: &AtomicBool,
    latency_ms: &Mutex<f32>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = match device_id {
        Some(id) => enumerator.get_device(id),
        None => enumerator.get_default_device(&Direction::Render),
    }
    .map_err(|e| format!("output device not available: {e}"))?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
    client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(|e| format!("could not open the output device: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    let buffer_frames = client.get_buffer_size().map_err(|e| e.to_string())?;
    *latency_ms.lock().unwrap() = buffer_frames as f32 * 1000.0 / 48_000.0;
    client.start_stream().map_err(|e| e.to_string())?;
    running.store(true, Ordering::Relaxed);

    let mut samples: Vec<f32> = Vec::with_capacity(buffer_frames as usize * 2);
    let mut bytes: Vec<u8> = Vec::with_capacity(buffer_frames as usize * 8);
    let mut volume;
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if event.wait_for_event(200).is_err() {
            continue;
        }
        let frames = match client.get_available_space_in_frames() {
            Ok(frames) => frames as usize,
            Err(e) => break Err(format!("playback stopped: {e}")),
        };
        if frames == 0 {
            continue;
        }
        samples.clear();
        samples.resize(frames * 2, 0.0);
        {
            let mut mix = mix.lock().unwrap();
            let now = Instant::now();
            mix.streams.retain(|_, slot| now - slot.last_write < Duration::from_secs(5));
            for slot in mix.streams.values_mut() {
                slot.buffer.mix_into(&mut samples);
            }
            volume = mix.volume;
        }
        // Squared, so the slider feels even across its range.
        let gain = volume * volume;
        bytes.clear();
        for sample in &samples {
            bytes.extend_from_slice(&(sample * gain).clamp(-1.0, 1.0).to_le_bytes());
        }
        if let Err(e) = render.write_to_device(frames, &bytes, None) {
            break Err(format!("playback stopped: {e}"));
        }
    };
    let _ = client.stop_stream();
    result
}
