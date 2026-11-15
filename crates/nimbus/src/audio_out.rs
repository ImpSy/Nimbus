//! Audio output through cpal (CoreAudio on macOS, ALSA → PipeWire on the
//! Steam Deck) behind a small jitter buffer that keeps latency bounded.
//!
//! The device is left at the rate it is already running at (we never
//! reconfigure the user's hardware); if that isn't 48 kHz the stream is
//! resampled here.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, ErrorKind, FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
    SupportedBufferSize,
};
use parking_lot::Mutex;

/// Automated / scripted runs (and `NIMBUS_MUTE=1`) never open an audio
/// device, so tests can't make noise.
pub fn muted() -> bool {
    [
        "NIMBUS_MUTE",
        "NIMBUS_EXIT_AFTER",
        "NIMBUS_SCREENSHOT",
        "NIMBUS_TEST_INPUT",
        "NIMBUS_TEST_NAV",
    ]
    .iter()
    .any(|v| std::env::var_os(v).is_some())
}

/// Interleaved stereo samples at the output rate, shared with the device
/// callback.
struct Jitter {
    samples: VecDeque<f32>,
    playing: bool,
    /// Samples to accumulate before (re)starting playback.
    prime: usize,
}

pub struct AudioOut {
    _stream: cpal::Stream,
    jitter: Arc<Mutex<Jitter>>,
    failed: Arc<AtomicBool>,
    resampler: Option<Resampler>,
    scratch: Vec<f32>,
    /// Above this many queued samples, incoming audio is dropped to catch up.
    max: usize,
}

impl AudioOut {
    /// Open the default output for interleaved stereo f32 at `input_rate`,
    /// targeting `buffer_ms` of buffering.
    pub fn open(input_rate: u32, buffer_ms: u32) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no audio output device")?;
        let def = device.default_output_config()?;
        let format = def.sample_format();
        let channels = def.channels().max(1);
        let mut rate = def.sample_rate();
        // ALSA's default device (PipeWire on the Deck) accepts any rate.
        if rate != input_rate && cfg!(target_os = "linux") {
            let supported = device.supported_output_configs().is_ok_and(|mut it| {
                it.any(|r| {
                    r.sample_format() == format
                        && r.channels() == channels
                        && (r.min_sample_rate()..=r.max_sample_rate()).contains(&input_rate)
                })
            });
            if supported {
                rate = input_rate;
            }
        }
        // ~10 ms device buffer.
        let buffer_size = match def.buffer_size() {
            SupportedBufferSize::Range { min, max } => {
                BufferSize::Fixed((rate / 100).clamp(*min, *max))
            }
            SupportedBufferSize::Unknown => BufferSize::Default,
        };

        let per_ms = rate as usize * 2 / 1000;
        let target = buffer_ms.max(10) as usize * per_ms;
        let jitter = Arc::new(Mutex::new(Jitter {
            samples: VecDeque::with_capacity(target * 4),
            playing: false,
            prime: target / 2,
        }));
        let failed = Arc::new(AtomicBool::new(false));

        let mut config = StreamConfig {
            channels,
            sample_rate: rate,
            buffer_size,
        };
        let stream = match build(&device, format, &config, &jitter, &failed) {
            Ok(s) => s,
            Err(e) if config.buffer_size != BufferSize::Default => {
                tracing::debug!("fixed audio buffer rejected ({e}), using the default");
                config.buffer_size = BufferSize::Default;
                build(&device, format, &config, &jitter, &failed)?
            }
            Err(e) => return Err(e),
        };
        stream.play()?;
        let name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_default();
        tracing::info!(
            device = %name,
            rate,
            channels,
            ?format,
            buffer = ?config.buffer_size,
            "audio output opened"
        );
        Ok(Self {
            _stream: stream,
            jitter,
            failed,
            resampler: (rate != input_rate).then(|| Resampler::new(input_rate, rate)),
            scratch: Vec::new(),
            max: target * 2 + 20 * per_ms,
        })
    }

    /// The device went away (and the backend couldn't reroute).
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Queue interleaved stereo samples at the input rate.
    pub fn push(&mut self, pcm: &[f32]) {
        let data = match &mut self.resampler {
            Some(r) => {
                self.scratch.clear();
                r.process(pcm, &mut self.scratch);
                &self.scratch[..]
            }
            None => pcm,
        };
        let mut j = self.jitter.lock();
        if j.samples.len() > self.max {
            // Too much latency accumulated: drop this packet to catch up.
            return;
        }
        j.samples.extend(data);
    }
}

fn build(
    device: &cpal::Device,
    format: SampleFormat,
    config: &StreamConfig,
    jitter: &Arc<Mutex<Jitter>>,
    failed: &Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    Ok(match format {
        SampleFormat::F32 => build_typed::<f32>(device, config, jitter, failed)?,
        SampleFormat::I16 => build_typed::<i16>(device, config, jitter, failed)?,
        SampleFormat::I32 => build_typed::<i32>(device, config, jitter, failed)?,
        SampleFormat::U16 => build_typed::<u16>(device, config, jitter, failed)?,
        other => bail!("unsupported output sample format {other:?}"),
    })
}

fn build_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    jitter: &Arc<Mutex<Jitter>>,
    failed: &Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let jitter = jitter.clone();
    let failed = failed.clone();
    device.build_output_stream::<T, _, _>(
        *config,
        move |out: &mut [T], _| fill(&jitter, out, channels),
        move |e| match e.kind() {
            ErrorKind::DeviceChanged => tracing::info!("audio output rerouted: {e}"),
            _ => {
                tracing::warn!("audio output error: {e}");
                failed.store(true, Ordering::Relaxed);
            }
        },
        None,
    )
}

/// Device callback: play from the jitter buffer, silence on underrun (and
/// wait for a small cushion before resuming).
fn fill<T: Sample + FromSample<f32>>(jitter: &Mutex<Jitter>, out: &mut [T], channels: usize) {
    let mut j = jitter.lock();
    if !j.playing && j.samples.len() >= j.prime.max(2) {
        j.playing = true;
    }
    for frame in out.chunks_mut(channels) {
        if !j.playing || j.samples.len() < 2 {
            j.playing = false;
            frame.fill(T::EQUILIBRIUM);
            continue;
        }
        let l = j.samples.pop_front().unwrap_or(0.0);
        let r = j.samples.pop_front().unwrap_or(0.0);
        match frame {
            [mono] => *mono = T::from_sample((l + r) * 0.5),
            [fl, fr, rest @ ..] => {
                *fl = T::from_sample(l);
                *fr = T::from_sample(r);
                rest.fill(T::EQUILIBRIUM);
            }
            [] => {}
        }
    }
}

/// Linear-interpolating stereo resampler, only used when the output device
/// doesn't run at the stream's rate.
struct Resampler {
    step: f64,
    /// Position in [prev, input...] (prev at index 0).
    pos: f64,
    prev: [f32; 2],
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            pos: 0.0,
            prev: [0.0; 2],
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        let prev = self.prev;
        let at = |i: usize| -> [f32; 2] {
            if i == 0 {
                prev
            } else {
                [input[2 * (i - 1)], input[2 * (i - 1) + 1]]
            }
        };
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push(a[0] + (b[0] - a[0]) * f);
            out.push(a[1] + (b[1] - a[1]) * f);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = at(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_rate_and_continuity() {
        let mut r = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        let mut t = 0usize;
        // 1 s of a 440 Hz tone in 10 ms packets.
        for _ in 0..100 {
            let pkt: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let v = ((t + i) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                    [v, -v]
                })
                .collect();
            t += 480;
            r.process(&pkt, &mut out);
        }
        let frames = out.len() / 2;
        assert!((44_098..=44_102).contains(&frames), "{frames}");
        // No discontinuities: a 440 Hz sine at 44.1 kHz moves < 0.07 per sample.
        let max_step = out
            .chunks(2)
            .zip(out.chunks(2).skip(1))
            .map(|(a, b)| (b[0] - a[0]).abs())
            .fold(0f32, f32::max);
        assert!(max_step < 0.07, "{max_step}");
        assert!(out.chunks(2).all(|f| (f[0] + f[1]).abs() < 1e-6));
    }

    #[test]
    fn jitter_buffer_primes_and_underruns() {
        let j = Mutex::new(Jitter {
            samples: VecDeque::new(),
            playing: false,
            prime: 8,
        });
        let mut out = [1.0f32; 8];
        j.lock().samples.extend([0.5f32; 6]);
        fill(&j, &mut out, 2);
        assert_eq!(out, [0.0; 8], "not primed yet");
        j.lock().samples.extend([0.5f32; 4]);
        fill(&j, &mut out, 2);
        assert_eq!(out, [0.5; 8]);
        fill(&j, &mut out, 2);
        assert_eq!(&out[..2], &[0.5, 0.5]);
        assert_eq!(&out[2..], &[0.0; 6], "underrun → silence");
        assert!(!j.lock().playing);
    }
}
