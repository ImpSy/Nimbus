//! A live streaming session: WebRTC transport + xCloud data channel
//! protocols.
//!
//! Threading model:
//! * the **network thread** (see [`rtc`]) owns the `str0m::Rtc`, the UDP
//!   sockets and all data-channel protocol state. It never blocks on anything
//!   but `mio::Poll`, so input packets are written to the wire as soon as they
//!   are produced (woken through a `mio::Waker`).
//! * encoded video / audio frames are handed to consumer threads through
//!   channels; decoding never happens on the network thread.

pub mod rtc;
pub mod session;

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Sender;

pub use session::{
    CodecPreference, SessionProgress, StreamConfig, StreamSession, start_stream,
};

use crate::protocol::input::{FrameMetadata, GamepadState, KeyEvent, Vibration};
use crate::protocol::messages::Dialog;

/// One encoded H.264 access unit (Annex-B).
#[derive(Clone)]
pub struct EncodedVideoFrame {
    pub data: Arc<[u8]>,
    pub rtp_timestamp: u32,
    /// Arrival time of the first packet of this frame.
    pub arrival: Instant,
    pub keyframe: bool,
    /// False when packets were lost before this frame.
    pub contiguous: bool,
}

/// One Opus packet.
#[derive(Clone)]
pub struct EncodedAudioPacket {
    pub data: Arc<[u8]>,
    pub rtp_timestamp: u32,
    pub arrival: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct NetworkStats {
    pub rtt_ms: Option<f32>,
    pub video_kbps: f32,
    pub audio_kbps: f32,
    pub loss_percent: f32,
    pub packets_lost_total: u64,
    pub nacks_total: u64,
    pub plis_total: u64,
    pub jitter_ms: f32,
    pub candidate: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// ICE + DTLS established.
    Connected,
    /// All data channels open and the server acknowledged our handshake.
    Ready,
    Disconnected(String),
    VideoSize { width: u32, height: u32 },
    Rumble(Vibration),
    Dialog(Dialog),
    TitleInfo(serde_json::Value),
    Stats(NetworkStats),
}

#[derive(Debug, Clone)]
pub enum Command {
    Gamepad(u8, GamepadState),
    GamepadConnected(u8, bool),
    FrameMetadata(FrameMetadata),
    Keys(Vec<KeyEvent>),
    RequestKeyframe,
    DialogResult { transaction_id: String, result: u32 },
    Shutdown,
}

/// Where the network thread delivers media and events.
#[derive(Clone)]
pub struct StreamSinks {
    pub video: Sender<EncodedVideoFrame>,
    pub audio: Sender<EncodedAudioPacket>,
    pub events: Sender<StreamEvent>,
}

/// Cheap, cloneable handle to talk to the network thread from any thread.
#[derive(Clone)]
pub struct StreamHandle {
    tx: Sender<Command>,
    waker: Arc<mio::Waker>,
    epoch: Instant,
}

impl StreamHandle {
    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            let _ = self.waker.wake();
        }
    }

    pub fn gamepad(&self, index: u8, state: GamepadState) {
        self.send(Command::Gamepad(index, state));
    }

    /// Session clock origin, used for frame metadata timestamps.
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Milliseconds since the stream epoch (wrapping u32, like the web client).
    pub fn ms(&self, t: Instant) -> u32 {
        t.saturating_duration_since(self.epoch).as_millis() as u32
    }
}
