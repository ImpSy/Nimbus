//! Media pipeline threads: video decode → render mailbox, audio decode →
//! audio output.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use nimbus_media::audio::{CHANNELS, OpusDecoder, SAMPLE_RATE};
use nimbus_media::video::{VideoDecoder, VideoFrame};
use parking_lot::Mutex;
use xcloud::stream::{Command, EncodedAudioPacket, EncodedVideoFrame};

use crate::audio_out::{self, AudioOut};
use crate::shared::Shared;

/// How long we hide corrupted video waiting for a keyframe after loss before
/// giving up and decoding anyway (the server may be using intra refresh).
const KEYFRAME_WAIT: Duration = Duration::from_millis(700);

pub struct DecodedPicture {
    pub frame: VideoFrame,
    pub rtp_timestamp: u32,
    pub arrival: Instant,
    pub decode_start: Instant,
    pub decoded: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct DecodeStats {
    pub backend: &'static str,
    pub decoded_frames: u64,
    pub skipped_frames: u64,
    pub dropped_for_keyframe: u64,
    pub decode_ms_avg: f32,
    pub decode_ms_max: f32,
    pub width: u32,
    pub height: u32,
}

/// Single-slot "latest frame wins" handoff to the renderer.
#[derive(Default)]
pub struct Mailbox {
    slot: Mutex<Option<DecodedPicture>>,
}

impl Mailbox {
    pub fn take(&self) -> Option<DecodedPicture> {
        self.slot.lock().take()
    }

    fn put(&self, p: DecodedPicture) -> bool {
        self.slot.lock().replace(p).is_some()
    }
}

pub struct VideoPipeline {
    pub mailbox: Arc<Mailbox>,
    pub stats: Arc<Mutex<DecodeStats>>,
    thread: Option<JoinHandle<()>>,
}

impl VideoPipeline {
    pub fn spawn(rx: Receiver<EncodedVideoFrame>, shared: Arc<Shared>, notify: Sender<()>) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let stats = Arc::new(Mutex::new(DecodeStats::default()));
        let thread = {
            let mailbox = mailbox.clone();
            let stats = stats.clone();
            std::thread::Builder::new()
                .name("video-decode".into())
                .spawn(move || video_thread(rx, shared, notify, mailbox, stats))
                .expect("spawn video thread")
        };
        Self {
            mailbox,
            stats,
            thread: Some(thread),
        }
    }

    /// Wait for the decode thread to finish (its input channel must be
    /// closed, which happens when the network thread exits).
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn request_keyframe(shared: &Shared) {
    if let Some(h) = shared.stream.lock().clone() {
        h.send(Command::RequestKeyframe);
    }
}

fn open_decoder() -> Option<VideoDecoder> {
    match VideoDecoder::new() {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::error!("cannot create video decoder: {e}");
            None
        }
    }
}

fn video_thread(
    rx: Receiver<EncodedVideoFrame>,
    shared: Arc<Shared>,
    notify: Sender<()>,
    mailbox: Arc<Mailbox>,
    stats: Arc<Mutex<DecodeStats>>,
) {
    raise_priority();
    let Some(mut dec) = open_decoder() else {
        return;
    };
    stats.lock().backend = dec.backend();

    let mut have_decoded = false;
    let mut waiting_for_keyframe: Option<Instant> = None;
    let mut avg = 0f32;

    while let Ok(f) = rx.recv() {
        let now = Instant::now();
        if f.keyframe {
            waiting_for_keyframe = None;
        } else if !f.contiguous && have_decoded {
            // Unrecoverable loss: hide the corruption until a keyframe.
            if waiting_for_keyframe.is_none() {
                tracing::debug!("frame loss, waiting for keyframe");
                waiting_for_keyframe = Some(now);
                request_keyframe(&shared);
            }
        }
        if let Some(since) = waiting_for_keyframe {
            if now.duration_since(since) < KEYFRAME_WAIT {
                stats.lock().dropped_for_keyframe += 1;
                continue;
            }
            tracing::debug!("no keyframe arrived, resuming decode");
            waiting_for_keyframe = None;
        }

        let decode_start = Instant::now();
        match dec.decode(&f.data, f.rtp_timestamp as i64) {
            Ok(Some(frame)) => {
                have_decoded = true;
                let decoded = Instant::now();
                let ms = decoded.duration_since(decode_start).as_secs_f32() * 1000.0;
                avg = if avg == 0.0 {
                    ms
                } else {
                    avg * 0.95 + ms * 0.05
                };
                {
                    let mut s = stats.lock();
                    s.decoded_frames += 1;
                    s.decode_ms_avg = avg;
                    s.decode_ms_max = s.decode_ms_max.max(ms) * 0.999;
                    s.width = frame.width;
                    s.height = frame.height;
                }
                // Behind? Keep decoding for references, but only show the newest.
                if !rx.is_empty() {
                    stats.lock().skipped_frames += 1;
                    continue;
                }
                let replaced = mailbox.put(DecodedPicture {
                    frame,
                    rtp_timestamp: f.rtp_timestamp,
                    arrival: f.arrival,
                    decode_start,
                    decoded,
                });
                if replaced {
                    stats.lock().skipped_frames += 1;
                }
                match notify.try_send(()) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Ok(None) => {}
            Err(e) => {
                if !have_decoded {
                    // Expected until the first keyframe with SPS/PPS shows up.
                    continue;
                }
                tracing::warn!("decode error: {e}");
                request_keyframe(&shared);
                waiting_for_keyframe = Some(Instant::now());
                // The Linux decoder may have fallen back to software.
                stats.lock().backend = dec.backend();
            }
        }
    }
    tracing::debug!("video decode thread exiting");
}

/// Audio: Opus → output device, with a bounded buffer to keep latency low.
pub struct AudioPipeline {
    thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    pub fn spawn(rx: Receiver<EncodedAudioPacket>, buffer_ms: u32) -> Self {
        let thread = std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || audio_thread(rx, buffer_ms))
            .expect("spawn audio thread");
        Self {
            thread: Some(thread),
        }
    }

    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn audio_thread(rx: Receiver<EncodedAudioPacket>, buffer_ms: u32) {
    debug_assert_eq!(CHANNELS, 2);
    let mut dec = match OpusDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("opus decoder: {e}");
            // Drain so the network thread never blocks on us.
            while rx.recv().is_ok() {}
            return;
        }
    };
    let muted = audio_out::muted();
    if muted {
        tracing::info!("audio muted (automated run or NIMBUS_MUTE)");
    }
    let mut out: Option<AudioOut> = None;
    let mut retry_at = Instant::now();
    let mut pcm = Vec::with_capacity(4096);
    while let Ok(p) = rx.recv() {
        pcm.clear();
        if let Err(e) = dec.decode(&p.data, &mut pcm) {
            tracing::debug!("opus: {e}");
            continue;
        }
        if muted {
            continue;
        }
        if out.as_ref().is_some_and(AudioOut::failed) {
            out = None;
            retry_at = Instant::now() + Duration::from_millis(500);
        }
        if out.is_none() && Instant::now() >= retry_at {
            match AudioOut::open(SAMPLE_RATE, buffer_ms) {
                Ok(o) => out = Some(o),
                Err(e) => {
                    tracing::error!("cannot open audio output: {e:#}");
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(o) = &mut out {
            o.push(&pcm);
        }
    }
    tracing::debug!("audio thread exiting");
}

fn raise_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
        }
        pthread_set_qos_class_self_np(0x21, 0);
    }
}
