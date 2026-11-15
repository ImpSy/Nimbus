//! The network thread: drives `str0m` (sans-IO WebRTC) over mio UDP sockets
//! and implements the xCloud data channel protocols.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token, Waker};
use str0m::channel::ChannelId;
use str0m::media::{KeyframeRequestKind, MediaData, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use super::{
    Command, EncodedAudioPacket, EncodedVideoFrame, NetworkStats, StreamEvent, StreamHandle,
    StreamSinks,
};
use crate::error::{Error, Result};
use crate::protocol::input::{
    FrameMetadata, GamepadState, InputEncoder, KeyEvent, ServerInput, parse_server_packet,
};
use crate::protocol::messages::{self, MessageEvent};

const WAKER_TOKEN: Token = Token(usize::MAX);
/// Resend the gamepad state at least this often (keeps the server in sync
/// even if it drops a report and matches the official client's cadence).
const GAMEPAD_HEARTBEAT: Duration = Duration::from_millis(16);
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(250);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const MAX_GAMEPADS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelKind {
    Chat,
    Control,
    Input,
    Message,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChannelIds {
    pub chat: ChannelId,
    pub control: ChannelId,
    pub input: ChannelId,
    pub message: ChannelId,
}

impl ChannelIds {
    fn kind(&self, id: ChannelId) -> Option<ChannelKind> {
        if id == self.chat {
            Some(ChannelKind::Chat)
        } else if id == self.control {
            Some(ChannelKind::Control)
        } else if id == self.input {
            Some(ChannelKind::Input)
        } else if id == self.message {
            Some(ChannelKind::Message)
        } else {
            None
        }
    }

    fn id(&self, kind: ChannelKind) -> ChannelId {
        match kind {
            ChannelKind::Chat => self.chat,
            ChannelKind::Control => self.control,
            ChannelKind::Input => self.input,
            ChannelKind::Message => self.message,
        }
    }
}

pub(crate) struct BoundSocket {
    pub socket: UdpSocket,
    pub local: SocketAddr,
}

/// Bind one UDP socket per local address with large kernel buffers
/// (keyframes at 1080p high bitrate arrive as bursts of hundreds of packets).
pub(crate) fn bind_sockets(addrs: &[IpAddr]) -> Vec<BoundSocket> {
    use socket2::{Domain, Socket, Type};
    let mut out = Vec::new();
    for ip in addrs {
        let domain = if ip.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let result = (|| -> std::io::Result<BoundSocket> {
            let s = Socket::new(domain, Type::DGRAM, Some(socket2::Protocol::UDP))?;
            if ip.is_ipv6() {
                s.set_only_v6(true)?;
            }
            let _ = s.set_recv_buffer_size(8 * 1024 * 1024);
            let _ = s.set_send_buffer_size(1024 * 1024);
            s.set_nonblocking(true)?;
            s.bind(&SocketAddr::new(*ip, 0).into())?;
            let std_sock: std::net::UdpSocket = s.into();
            let local = std_sock.local_addr()?;
            Ok(BoundSocket {
                socket: UdpSocket::from_std(std_sock),
                local,
            })
        })();
        match result {
            Ok(b) => {
                tracing::debug!(local = %b.local, "bound UDP socket");
                out.push(b)
            }
            Err(e) => tracing::warn!(%ip, "failed to bind UDP socket: {e}"),
        }
    }
    out
}

/// Learn the public (server-reflexive) address of each IPv4 socket via STUN.
/// Best effort: returns whatever answered within `timeout`.
pub(crate) async fn stun_mappings(
    sockets: &[BoundSocket],
    timeout: Duration,
) -> Vec<(SocketAddr, SocketAddr)> {
    use crate::ice::stun;
    let servers: Vec<SocketAddr> = tokio::task::spawn_blocking(|| {
        use std::net::ToSocketAddrs;
        stun::SERVERS
            .iter()
            .filter_map(|s| s.to_socket_addrs().ok())
            .flat_map(|it| it.filter(|a| a.is_ipv4()).take(1))
            .collect()
    })
    .await
    .unwrap_or_default();
    if servers.is_empty() {
        return Vec::new();
    }
    let mut pending = Vec::new();
    for (i, s) in sockets
        .iter()
        .enumerate()
        .filter(|(_, s)| s.local.is_ipv4())
    {
        let tid: [u8; 12] = *uuid::Uuid::new_v4().as_bytes().first_chunk().unwrap();
        for server in &servers {
            let _ = s.socket.send_to(&stun::binding_request(tid), *server);
        }
        pending.push((i, tid));
    }
    let mut found = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    while !pending.is_empty() && Instant::now() < deadline {
        pending.retain(|(i, tid)| {
            let s = &sockets[*i];
            while let Ok((n, _)) = s.socket.recv_from(&mut buf) {
                if let Some(mapped) = stun::parse_response(&buf[..n], tid) {
                    found.push((mapped, s.local));
                    return false;
                }
            }
            true
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    found
}

/// Drain `poll_output` until a timeout, transmitting packets and
/// collecting events (used during setup, before the thread runs).
pub(crate) fn drain_setup(rtc: &mut Rtc, events: &mut Vec<Event>) -> Result<()> {
    loop {
        match rtc.poll_output()? {
            Output::Timeout(_) => return Ok(()),
            Output::Transmit(_) => {} // nothing to transmit before candidates exist
            Output::Event(e) => events.push(e),
        }
    }
}

pub(crate) struct DriverInit {
    pub rtc: Rtc,
    pub sockets: Vec<BoundSocket>,
    pub remote_candidates: Vec<Candidate>,
    pub channels: ChannelIds,
    pub audio_mid: Mid,
    pub video_mid: Mid,
    pub pending_events: Vec<Event>,
    pub sinks: StreamSinks,
    pub video_size: (u32, u32),
    /// Negotiated input protocol version.
    pub input_version: u8,
}

struct Driver {
    rtc: Rtc,
    poll: Poll,
    sockets: Vec<BoundSocket>,
    cmd_rx: Receiver<Command>,
    sinks: StreamSinks,
    channels: ChannelIds,
    audio_mid: Mid,
    video_mid: Mid,
    epoch: Instant,
    video_size: (u32, u32),

    open: Vec<ChannelKind>,
    outbox: VecDeque<(ChannelKind, bool, Vec<u8>)>,
    handshake_done: bool,
    connected: bool,
    shutdown: bool,

    encoder: InputEncoder,
    pads: [Option<GamepadState>; MAX_GAMEPADS],
    pads_dirty: bool,
    last_gamepad_send: Instant,
    pending_metadata: Vec<FrameMetadata>,
    pending_keys: Vec<KeyEvent>,

    last_keyframe_request: Option<Instant>,

    stats: NetworkStats,
    last_video_stats: Option<(u64, Instant)>,
    last_audio_stats: Option<(u64, Instant)>,
    last_stats_emit: Instant,
    buf: Vec<u8>,
}

/// Spawn the network thread. Returns a handle for sending commands.
pub(crate) fn spawn(init: DriverInit) -> Result<(StreamHandle, std::thread::JoinHandle<()>)> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN)?);
    let (cmd_tx, cmd_rx) = unbounded();
    let epoch = Instant::now();

    let mut sockets = init.sockets;
    for (i, s) in sockets.iter_mut().enumerate() {
        poll.registry()
            .register(&mut s.socket, Token(i), Interest::READABLE)?;
    }

    let mut driver = Driver {
        rtc: init.rtc,
        poll,
        sockets,
        cmd_rx,
        sinks: init.sinks,
        channels: init.channels,
        audio_mid: init.audio_mid,
        video_mid: init.video_mid,
        epoch,
        video_size: init.video_size,
        open: Vec::new(),
        outbox: VecDeque::new(),
        handshake_done: false,
        connected: false,
        shutdown: false,
        encoder: InputEncoder::with_version(init.input_version),
        pads: [None; MAX_GAMEPADS],
        pads_dirty: false,
        last_gamepad_send: epoch,
        pending_metadata: Vec::new(),
        pending_keys: Vec::new(),
        last_keyframe_request: None,
        stats: NetworkStats::default(),
        last_video_stats: None,
        last_audio_stats: None,
        last_stats_emit: epoch,
        buf: vec![0u8; 2048],
    };
    // Gamepad 0 always exists (keyboard / first controller).
    driver.pads[0] = Some(GamepadState::default());

    let handle = StreamHandle {
        tx: cmd_tx,
        waker,
        epoch,
    };

    let pending = init.pending_events;
    let remote = init.remote_candidates;
    let join = std::thread::Builder::new()
        .name("xcloud-net".into())
        .spawn(move || {
            raise_thread_priority();
            if let Err(e) = driver.run(pending, remote) {
                tracing::error!("network thread stopped: {e}");
                let _ = driver
                    .sinks
                    .events
                    .send(StreamEvent::Disconnected(e.to_string()));
            }
        })?;
    Ok((handle, join))
}

fn raise_thread_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        // QOS_CLASS_USER_INTERACTIVE
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
        }
        pthread_set_qos_class_self_np(0x21, 0);
    }
}

impl Driver {
    fn ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    fn run(&mut self, pending: Vec<Event>, remote: Vec<Candidate>) -> Result<()> {
        for e in pending {
            self.handle_event(e);
        }
        for c in remote {
            tracing::info!(candidate = %c, "adding remote candidate");
            self.rtc.add_remote_candidate(c);
            self.drain()?;
        }

        let mut events = Events::with_capacity(64);
        let mut next_timeout = self.drain()?;
        while !self.shutdown {
            let now = Instant::now();
            let mut deadline = next_timeout;
            if self.handshake_done {
                deadline = deadline.min(self.last_gamepad_send + GAMEPAD_HEARTBEAT);
            }
            let wait = deadline.saturating_duration_since(now);
            match self.poll.poll(&mut events, Some(wait)) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            for ev in events.iter() {
                match ev.token() {
                    WAKER_TOKEN => {
                        if let Some(t) = self.handle_commands()? {
                            next_timeout = t;
                        }
                    }
                    Token(i) => {
                        if let Some(t) = self.read_socket(i)? {
                            next_timeout = t;
                        }
                    }
                }
            }

            let now = Instant::now();
            if now >= next_timeout {
                self.rtc.handle_input(Input::Timeout(now))?;
                next_timeout = self.drain()?;
            }
            if self.handshake_done && now >= self.last_gamepad_send + GAMEPAD_HEARTBEAT {
                self.flush_input(true);
                next_timeout = self.drain()?;
            }
            if now >= self.last_stats_emit + STATS_INTERVAL {
                self.last_stats_emit = now;
                let _ = self
                    .sinks
                    .events
                    .send(StreamEvent::Stats(self.stats.clone()));
            }
        }

        self.rtc.disconnect();
        let _ = self.drain();
        Ok(())
    }

    fn read_socket(&mut self, i: usize) -> Result<Option<Instant>> {
        let mut timeout = None;
        loop {
            let Some(sock) = self.sockets.get(i) else {
                return Ok(timeout);
            };
            let local = sock.local;
            match sock.socket.recv_from(&mut self.buf) {
                Ok((n, source)) => {
                    let Ok(contents) = self.buf[..n].try_into() else {
                        continue;
                    };
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local,
                            contents,
                        },
                    );
                    if self.rtc.accepts(&input) {
                        if let Err(e) = self.rtc.handle_input(input) {
                            tracing::debug!("rtc rejected packet: {e}");
                        }
                        timeout = Some(self.drain()?);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(timeout),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    // ICMP port unreachable etc. on some platforms; not fatal.
                    tracing::debug!(%local, "recv error: {e}");
                    return Ok(timeout);
                }
            }
        }
    }

    fn handle_commands(&mut self) -> Result<Option<Instant>> {
        let mut input_changed = false;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Command::Gamepad(i, state) => {
                    // States for unannounced slots are ignored (the slot must
                    // be added with GamepadConnected first).
                    if let Some(slot) = self.pads.get_mut(i as usize)
                        && slot.is_some()
                        && *slot != Some(state)
                    {
                        *slot = Some(state);
                        self.pads_dirty = true;
                        input_changed = true;
                    }
                }
                Command::GamepadConnected(i, added) => {
                    if let Some(slot) = self.pads.get_mut(i as usize) {
                        let was = slot.is_some();
                        // Slot 0 always stays registered (keyboard / first pad).
                        let now_present = added || i == 0;
                        if now_present && !was {
                            *slot = Some(GamepadState::default());
                        } else if !now_present {
                            *slot = None;
                        }
                        if was != now_present && self.handshake_done {
                            self.send(
                                ChannelKind::Control,
                                false,
                                messages::control_gamepad_changed(i, now_present).into_bytes(),
                            );
                        }
                    }
                }
                Command::FrameMetadata(m) => {
                    self.pending_metadata.push(m);
                    if self.pending_metadata.len() >= 6 {
                        input_changed = true;
                    }
                }
                Command::Keys(k) => {
                    self.pending_keys.extend(k);
                    input_changed = true;
                }
                Command::RequestKeyframe => self.request_keyframe(true),
                Command::DialogResult {
                    transaction_id,
                    result,
                } => {
                    self.send(
                        ChannelKind::Message,
                        false,
                        messages::transaction_complete(
                            &transaction_id,
                            serde_json::json!({ "Result": result }),
                        )
                        .into_bytes(),
                    );
                }
                Command::Shutdown => self.shutdown = true,
            }
        }
        if input_changed && self.handshake_done {
            self.flush_input(false);
        }
        Ok(Some(self.drain()?))
    }

    /// Send pending input. Gamepad states are always included when dirty or
    /// when `heartbeat` is set.
    fn flush_input(&mut self, heartbeat: bool) {
        let include_pads = self.pads_dirty || heartbeat;
        let pads: Vec<(u8, GamepadState)> = if include_pads {
            self.pads
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.map(|s| (i as u8, s)))
                .collect()
        } else {
            Vec::new()
        };
        let ts = self.ms();
        let metadata = std::mem::take(&mut self.pending_metadata);
        let keys = std::mem::take(&mut self.pending_keys);
        if let Some(pkt) = self.encoder.encode(ts, &metadata, &pads, &keys) {
            self.send(ChannelKind::Input, true, pkt);
        }
        if include_pads {
            self.pads_dirty = false;
            self.last_gamepad_send = Instant::now();
        }
    }

    fn send(&mut self, kind: ChannelKind, binary: bool, data: Vec<u8>) {
        if !self.open.contains(&kind) {
            // Input is real-time: don't queue stale reports for a channel
            // that isn't up (or went away).
            if (kind != ChannelKind::Input || !self.handshake_done) && self.outbox.len() < 256 {
                self.outbox.push_back((kind, binary, data));
            }
            return;
        }
        let id = self.channels.id(kind);
        match self.rtc.channel(id) {
            Some(mut ch) => match ch.write(binary, &data) {
                Ok(true) => {}
                Ok(false) => {
                    // SCTP send buffer full. Input reports are superseded by
                    // the next one anyway; everything else is retried.
                    if kind != ChannelKind::Input && self.outbox.len() < 256 {
                        self.outbox.push_back((kind, binary, data));
                    }
                }
                Err(e) => tracing::warn!(?kind, "channel write failed: {e}"),
            },
            None => {
                self.open.retain(|k| *k != kind);
                if kind != ChannelKind::Input && self.outbox.len() < 256 {
                    self.outbox.push_back((kind, binary, data));
                }
            }
        }
    }

    fn flush_outbox(&mut self, kind: ChannelKind) {
        let pending = std::mem::take(&mut self.outbox);
        for (k, binary, data) in pending {
            if k == kind {
                self.send(k, binary, data);
            } else {
                self.outbox.push_back((k, binary, data));
            }
        }
    }

    fn request_keyframe(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_keyframe_request
                .is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_MIN_INTERVAL)
        {
            return;
        }
        self.last_keyframe_request = Some(now);
        tracing::debug!("requesting keyframe");
        if let Some(rx) = self.rtc.direct_api().stream_rx_by_mid(self.video_mid, None) {
            rx.request_keyframe(KeyframeRequestKind::Pli);
        }
        if self.handshake_done {
            self.send(
                ChannelKind::Control,
                false,
                messages::control_keyframe_request(false).into_bytes(),
            );
        }
    }

    /// Drain `poll_output` until it yields a timeout.
    fn drain(&mut self) -> Result<Instant> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => return Ok(t),
                Output::Transmit(t) => {
                    let sock = self
                        .sockets
                        .iter()
                        .find(|s| s.local == t.source)
                        .or_else(|| {
                            self.sockets
                                .iter()
                                .find(|s| s.local.is_ipv4() == t.destination.is_ipv4())
                        });
                    if let Some(s) = sock
                        && let Err(e) = s.socket.send_to(&t.contents, t.destination)
                    {
                        tracing::trace!(dest = %t.destination, "send failed: {e}");
                    }
                }
                Output::Event(e) => self.handle_event(e),
            }
        }
    }

    fn handle_event(&mut self, e: Event) {
        match e {
            Event::IceConnectionStateChange(state) => {
                tracing::info!(?state, "ICE state");
                if state == IceConnectionState::Disconnected {
                    let _ = self
                        .sinks
                        .events
                        .send(StreamEvent::Disconnected("Network connection lost".into()));
                    self.shutdown = true;
                }
            }
            Event::Connected => {
                tracing::info!("WebRTC connected (DTLS up)");
                self.connected = true;
                let _ = self.sinks.events.send(StreamEvent::Connected);
            }
            Event::ChannelOpen(id, label) => {
                tracing::info!(%label, "data channel open");
                let Some(kind) = self.channels.kind(id) else {
                    return;
                };
                self.open.push(kind);
                if kind == ChannelKind::Message {
                    self.send(
                        ChannelKind::Message,
                        false,
                        messages::message_handshake().into_bytes(),
                    );
                }
                self.flush_outbox(kind);
            }
            Event::ChannelData(d) => match self.channels.kind(d.id) {
                Some(ChannelKind::Message) => self.on_message(&d.data),
                Some(ChannelKind::Input) => {
                    match parse_server_packet(&d.data, self.encoder.version()) {
                        Some(ServerInput::Vibration(v)) => {
                            let _ = self.sinks.events.send(StreamEvent::Rumble(v));
                        }
                        Some(ServerInput::VideoSize { width, height }) => {
                            let _ = self
                                .sinks
                                .events
                                .send(StreamEvent::VideoSize { width, height });
                        }
                        None => {}
                    }
                }
                Some(ChannelKind::Control) => {
                    tracing::debug!(data = %String::from_utf8_lossy(&d.data), "control message");
                }
                _ => {}
            },
            Event::ChannelClose(id) => {
                if let Some(kind) = self.channels.kind(id) {
                    self.open.retain(|k| *k != kind);
                }
                if matches!(
                    self.channels.kind(id),
                    Some(ChannelKind::Message | ChannelKind::Control)
                ) && !self.shutdown
                {
                    let _ = self.sinks.events.send(StreamEvent::Disconnected(
                        "The console closed the stream".into(),
                    ));
                    self.shutdown = true;
                }
            }
            Event::MediaData(d) => self.on_media(d),
            Event::ChannelBufferedAmountLow(id) => {
                if let Some(kind) = self.channels.kind(id) {
                    self.flush_outbox(kind);
                }
            }
            Event::MediaIngressStats(s) => {
                let now = s.timestamp;
                let prev = if s.mid == self.video_mid {
                    &mut self.last_video_stats
                } else {
                    &mut self.last_audio_stats
                };
                let kbps = match prev {
                    Some((bytes, t)) if now > *t => {
                        let secs = now.duration_since(*t).as_secs_f32();
                        (s.bytes.saturating_sub(*bytes) as f32 * 8.0 / 1000.0) / secs
                    }
                    _ => 0.0,
                };
                *prev = Some((s.bytes, now));
                if s.mid == self.video_mid {
                    self.stats.video_kbps = kbps;
                    self.stats.nacks_total = s.nacks;
                    self.stats.plis_total = s.plis;
                    self.stats.loss_percent = s.loss.unwrap_or(0.0) * 100.0;
                    self.stats.jitter_ms = s.jitter as f32 / 90.0;
                    if let Some(rtt) = s.rtt {
                        self.stats.rtt_ms = Some(rtt.as_secs_f32() * 1000.0);
                    }
                } else {
                    self.stats.audio_kbps = kbps;
                }
            }
            Event::PeerStats(p) => {
                if let Some(rtt) = p.rtt {
                    self.stats.rtt_ms = Some(rtt.as_secs_f32() * 1000.0);
                }
                if let Some(pair) = p.selected_candidate_pair {
                    self.stats.candidate = Some(format!("{pair:?}"));
                }
            }
            Event::MediaAdded(m) => tracing::debug!(mid = %m.mid, kind = ?m.kind, "media added"),
            _ => {}
        }
    }

    fn on_media(&mut self, d: MediaData) {
        let rtp_timestamp = d.time.numer() as u32;
        if d.mid == self.video_mid {
            let keyframe =
                matches!(d.codec_extra, str0m::format::CodecExtra::H264(e) if e.is_keyframe);
            if !d.contiguous && !keyframe {
                self.stats.packets_lost_total += 1;
                self.request_keyframe(false);
            }
            let _ = self.sinks.video.send(EncodedVideoFrame {
                data: d.data,
                rtp_timestamp,
                arrival: d.network_time,
                keyframe,
                contiguous: d.contiguous,
            });
        } else if d.mid == self.audio_mid {
            let _ = self.sinks.audio.send(EncodedAudioPacket {
                data: d.data,
                rtp_timestamp,
                arrival: d.network_time,
            });
        }
    }

    fn on_message(&mut self, data: &[u8]) {
        tracing::debug!(msg = %String::from_utf8_lossy(data), "message channel");
        match messages::parse_message(data) {
            Some(MessageEvent::HandshakeAck) => {
                tracing::info!("message handshake acknowledged; starting input");
                self.handshake_done = true;
                self.send(
                    ChannelKind::Control,
                    false,
                    messages::control_authorization().into_bytes(),
                );
                for i in 0..MAX_GAMEPADS {
                    if self.pads[i].is_some() {
                        self.send(
                            ChannelKind::Control,
                            false,
                            messages::control_gamepad_changed(i as u8, true).into_bytes(),
                        );
                    }
                }
                let ts = self.ms();
                let meta = self.encoder.client_metadata(1, ts);
                self.send(ChannelKind::Input, true, meta);
                let install_id = uuid::Uuid::new_v4().to_string();
                for m in messages::initial_configuration(
                    self.video_size.0,
                    self.video_size.1,
                    &install_id,
                ) {
                    self.send(ChannelKind::Message, false, m.into_bytes());
                }
                // Start from a clean picture.
                self.request_keyframe(true);
                let _ = self.sinks.events.send(StreamEvent::Ready);
            }
            Some(MessageEvent::ServerDisconnect { transaction_id }) => {
                if let Some(id) = transaction_id {
                    self.send(
                        ChannelKind::Message,
                        false,
                        messages::transaction_complete(&id, serde_json::json!("")).into_bytes(),
                    );
                }
                let _ = self.sinks.events.send(StreamEvent::Disconnected(
                    "The server ended the session".into(),
                ));
                self.shutdown = true;
            }
            Some(MessageEvent::TitleInfo(v)) => {
                let _ = self.sinks.events.send(StreamEvent::TitleInfo(v));
            }
            Some(MessageEvent::Dialog(d)) => {
                let _ = self.sinks.events.send(StreamEvent::Dialog(d));
            }
            Some(MessageEvent::Other) | None => {}
        }
    }
}

impl From<std::net::AddrParseError> for Error {
    fn from(e: std::net::AddrParseError) -> Self {
        Error::Rtc(e.to_string())
    }
}
