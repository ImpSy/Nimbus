//! Opus decoding with a pure-Rust decoder (no FFI).

use crate::MediaError;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

pub struct OpusDecoder {
    inner: opus_decoder::OpusDecoder,
    buf: Vec<f32>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, MediaError> {
        let inner = opus_decoder::OpusDecoder::new(SAMPLE_RATE, CHANNELS as usize)
            .map_err(|e| MediaError::Init(format!("opus: {e:?}")))?;
        let max = inner.max_frame_size_per_channel() * CHANNELS as usize;
        Ok(Self {
            inner,
            buf: vec![0.0; max],
        })
    }

    /// Decode one packet, appending interleaved stereo f32 samples to `out`.
    /// An empty packet runs packet-loss concealment. Returns the number of
    /// frames (samples per channel) decoded.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<usize, MediaError> {
        let n = self
            .inner
            .decode_float(data, &mut self.buf, false)
            .map_err(|e| MediaError::Decode(format!("opus: {e:?}")))?;
        out.extend_from_slice(&self.buf[..n * CHANNELS as usize]);
        Ok(n)
    }
}
