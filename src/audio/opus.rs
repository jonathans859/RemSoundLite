//! Small safe wrappers over libopus (statically linked through `audiopus_sys`).
//! Always 48 kHz stereo interleaved float, which is what RemSound streams.

use audiopus_sys as ffi;

pub const SAMPLE_RATE: i32 = 48_000;
pub const CHANNELS: i32 = 2;
/// The largest Opus frame, 120 ms at 48 kHz, per channel.
const MAX_FRAME_SAMPLES: usize = 5760;

pub struct Encoder {
    raw: *mut ffi::OpusEncoder,
    frame_samples: i32,
}

// `frame_samples` is checked against every frame handed to `encode`.

// The encoder is only ever used from one thread at a time.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Low-delay CELT with in-band FEC, as RemSound configures it.
    pub fn new(frame_samples: i32, bitrate: i32) -> Result<Self, String> {
        let mut error = 0;
        let raw = unsafe {
            ffi::opus_encoder_create(SAMPLE_RATE, CHANNELS, ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY, &mut error)
        };
        if raw.is_null() || error != ffi::OPUS_OK {
            return Err(format!("could not create the Opus encoder (error {error})"));
        }
        unsafe {
            ffi::opus_encoder_ctl(raw, ffi::OPUS_SET_BITRATE_REQUEST, bitrate);
            ffi::opus_encoder_ctl(raw, ffi::OPUS_SET_INBAND_FEC_REQUEST, 1i32);
            ffi::opus_encoder_ctl(raw, ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10i32);
        }
        Ok(Self { raw, frame_samples })
    }

    /// Encodes exactly one frame of interleaved stereo samples into `out` (cleared first).
    pub fn encode(&mut self, frame: &[f32], out: &mut Vec<u8>) -> Result<(), i32> {
        debug_assert_eq!(frame.len(), (self.frame_samples * CHANNELS) as usize);
        out.clear();
        out.resize(1500, 0);
        let written = unsafe {
            ffi::opus_encode_float(self.raw, frame.as_ptr(), self.frame_samples, out.as_mut_ptr(), out.len() as i32)
        };
        if written < 0 {
            out.clear();
            return Err(written);
        }
        out.truncate(written as usize);
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_encoder_destroy(self.raw) }
    }
}

pub struct Decoder {
    raw: *mut ffi::OpusDecoder,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let raw = unsafe { ffi::opus_decoder_create(SAMPLE_RATE, CHANNELS, &mut error) };
        if raw.is_null() || error != ffi::OPUS_OK {
            return Err(format!("could not create the Opus decoder (error {error})"));
        }
        Ok(Self { raw })
    }

    /// Decodes one packet into `out` (replacing its contents). With `fec`, rebuilds the frame
    /// *before* this packet from the redundancy it carries; `frame_samples` must then be the
    /// lost frame's length. `packet` of `None` asks for packet-loss concealment.
    pub fn decode(&mut self, packet: Option<&[u8]>, frame_samples: i32, fec: bool, out: &mut Vec<f32>) -> Result<(), i32> {
        let capacity = if fec { frame_samples as usize } else { MAX_FRAME_SAMPLES };
        out.clear();
        out.resize(capacity * CHANNELS as usize, 0.0);
        let (data, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        let samples = unsafe {
            ffi::opus_decode_float(self.raw, data, len, out.as_mut_ptr(), capacity as i32, fec as i32)
        };
        if samples < 0 {
            out.clear();
            return Err(samples);
        }
        out.truncate(samples as usize * CHANNELS as usize);
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { ffi::opus_decoder_destroy(self.raw) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_a_tone() {
        let mut encoder = Encoder::new(480, 192_000).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut packet = Vec::new();
        let mut pcm = Vec::new();
        let mut energy = 0.0f32;
        for block in 0..20 {
            let frame: Vec<f32> = (0..480 * 2)
                .map(|i| {
                    let t = (block * 480 + i / 2) as f32 / 48_000.0;
                    (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
                })
                .collect();
            encoder.encode(&frame, &mut packet).unwrap();
            decoder.decode(Some(&packet), 480, false, &mut pcm).unwrap();
            assert_eq!(pcm.len(), 960);
            if block > 5 {
                energy += pcm.iter().map(|s| s * s).sum::<f32>();
            }
        }
        assert!(energy > 100.0, "decoded audio should carry the tone, energy {energy}");
    }
}
