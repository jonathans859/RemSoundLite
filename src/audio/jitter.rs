//! The receiver's playout (jitter) buffer for one incoming stream on one output device.
//!
//! Decoded 48 kHz stereo samples queue here; the render thread drains them. Playback starts
//! ("arms") once enough audio is buffered, plays a short concealment sound when it runs dry,
//! and waits to refill before resuming, so network hiccups become brief gaps rather than
//! drift.
//!
//! * `target_ms` is the jitter buffer setting.
//! * Smoothness (1-10, default 3) scales how much has to be buffered again after a gap:
//!   3 refills to the target, 10 to more than three times it (fewer repeat gaps, longer
//!   delay), 1 to a third of it (shortest delay, more gaps).
//! * The artefact decides what a gap sounds like: a short noise burst at the level the audio
//!   stopped at, or a plain cut to silence (a click).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

const RATE: usize = 48_000;
const CHANNELS: usize = 2;
/// Frames of noise played into a gap before falling silent.
const NOISE_FRAMES: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Artefact {
    #[default]
    NoiseBurst,
    Click,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BufferParams {
    pub target_ms: u32,
    pub smoothness: u8,
    pub artefact: Artefact,
}

pub struct PlayoutBuffer {
    samples: VecDeque<f32>,
    params: BufferParams,
    armed: bool,
    /// Last sample values played, per channel: the level a noise burst starts from.
    last: [f32; CHANNELS],
    noise_left: usize,
    noise_state: u32,
    pub underruns: u64,
    pub dropped_samples: u64,
}

fn samples_for_ms(ms: u32) -> usize {
    RATE * ms as usize / 1000 * CHANNELS
}

impl PlayoutBuffer {
    pub fn new(params: BufferParams) -> Self {
        Self {
            samples: VecDeque::new(),
            params,
            armed: false,
            last: [0.0; CHANNELS],
            noise_left: 0,
            noise_state: 0x1234_5678,
            underruns: 0,
            dropped_samples: 0,
        }
    }

    pub fn set_params(&mut self, params: BufferParams) {
        self.params = params;
    }

    /// How much must be buffered before playback (re)starts.
    fn arm_level(&self) -> usize {
        let smoothness = self.params.smoothness.clamp(1, 10) as u32;
        samples_for_ms((self.params.target_ms.max(5) * smoothness / 3).max(5))
    }

    /// Beyond this the sender has run ahead (a burst, or clock drift), and the oldest audio
    /// is dropped back to the arm level.
    fn cap(&self) -> usize {
        self.arm_level() + samples_for_ms(self.params.target_ms + 120)
    }

    #[cfg(test)]
    fn buffered_ms(&self) -> u32 {
        (self.samples.len() / CHANNELS * 1000 / RATE) as u32
    }

    pub fn write(&mut self, samples: &[f32]) {
        self.samples.extend(samples.iter().copied());
        if self.samples.len() > self.cap() {
            let keep = self.arm_level();
            let drop = self.samples.len() - keep;
            self.samples.drain(..drop);
            self.dropped_samples += drop as u64;
        }
        if !self.armed && self.samples.len() >= self.arm_level() {
            self.armed = true;
            self.noise_left = 0;
        }
    }

    fn noise(&mut self) -> f32 {
        // xorshift32: cheap, allocation-free and good enough for a burst of hiss.
        let mut x = self.noise_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.noise_state = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Adds the next `out.len()` samples into `out` (mixing with what's already there).
    /// Returns whether any real audio was played.
    pub fn mix_into(&mut self, out: &mut [f32]) -> bool {
        let mut played = false;
        let mut index = 0;
        if self.armed {
            let available = self.samples.len().min(out.len());
            for slot in out.iter_mut().take(available) {
                let sample = self.samples.pop_front().unwrap_or(0.0);
                *slot += sample;
                self.last[index % CHANNELS] = sample;
                index += 1;
            }
            played = available > 0;
            if available < out.len() {
                self.armed = false;
                self.underruns += 1;
                if self.params.artefact == Artefact::NoiseBurst {
                    self.noise_left = NOISE_FRAMES;
                }
            }
        }
        // Whatever is left is a gap: a fading noise burst, then silence.
        while index + CHANNELS <= out.len() && self.noise_left > 0 {
            let fade = self.noise_left as f32 / NOISE_FRAMES as f32;
            for channel in 0..CHANNELS {
                let level = self.last[channel].abs().min(0.5);
                out[index + channel] += self.noise() * level * fade;
            }
            index += CHANNELS;
            self.noise_left -= 1;
        }
        if !self.armed && self.noise_left == 0 {
            self.last = [0.0; CHANNELS];
        }
        played
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(target_ms: u32, smoothness: u8, artefact: Artefact) -> BufferParams {
        BufferParams { target_ms, smoothness, artefact }
    }

    #[test]
    fn arms_at_target_and_conceals_a_gap() {
        let mut buffer = PlayoutBuffer::new(params(20, 3, Artefact::Click));
        let mut out = vec![0.0f32; 960];
        buffer.write(&[0.5; 960]); // 10 ms: not armed yet
        assert!(!buffer.mix_into(&mut out));
        assert!(out.iter().all(|s| *s == 0.0));
        buffer.write(&[0.5; 960]); // 20 ms: armed
        assert!(buffer.mix_into(&mut out));
        assert!(out.iter().all(|s| *s == 0.5));
        out.fill(0.0);
        assert!(buffer.mix_into(&mut out));
        out.fill(0.0);
        assert!(!buffer.mix_into(&mut out));
        assert_eq!(buffer.underruns, 1);
        assert!(out.iter().all(|s| *s == 0.0), "a click gap is plain silence");
    }

    #[test]
    fn noise_burst_fills_the_start_of_a_gap() {
        let mut buffer = PlayoutBuffer::new(params(10, 3, Artefact::NoiseBurst));
        buffer.write(&[0.4; 960]);
        let mut out = vec![0.0f32; 960 + 200];
        buffer.mix_into(&mut out);
        assert!(out[960..960 + 64].iter().any(|s| s.abs() > 0.0), "noise right after the audio stops");
        assert!(out[960 + 64..].iter().all(|s| *s == 0.0), "then silence");
    }

    #[test]
    fn smoothness_scales_the_refill_and_runaway_is_capped() {
        let smooth = PlayoutBuffer::new(params(30, 10, Artefact::Click));
        let tight = PlayoutBuffer::new(params(30, 1, Artefact::Click));
        assert!(smooth.arm_level() > tight.arm_level() * 5);
        let mut buffer = PlayoutBuffer::new(params(40, 3, Artefact::Click));
        for _ in 0..40 {
            buffer.write(&[0.1; 960]);
        }
        assert!(buffer.buffered_ms() <= 40 + 40 + 120);
        assert!(buffer.dropped_samples > 0);
    }
}
