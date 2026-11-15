//! A local stand-in for the xCloud / xHome streaming backend, for end-to-end
//! testing without a console or Microsoft account.
//!
//! * serves the GSSV REST endpoints used by the client (plain HTTP);
//! * answers the client's WebRTC offer with str0m and streams a looping
//!   H.264 file (Annex-B with AUD NALs) + an Opus file;
//! * speaks the data-channel protocols: handshake ack, rumble when (A) is
//!   pressed, a console dialog when (Y) is pressed, logs input.
//!
//! ```text
//! scripts/mock-media.sh
//! cargo run -p xcloud --example mock_server -- target/mock-media 8085
//! NIMBUS_MOCK=http://127.0.0.1:8085 cargo run -p nimbus
//! ```

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};
use xcloud::protocol::input::{button, report};

#[derive(Default)]
struct Shared {
    offer: Option<String>,
    answer: Option<String>,
    remote_candidates: Vec<String>,
    local_candidate: Option<String>,
    polls: u32,
    session: u32,
}

fn split_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < data.len() {
        let sc3 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let sc4 = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        if sc3 || sc4 {
            let hdr = if sc4 { i + 4 } else { i + 3 };
            if data[hdr] & 0x1f == 9 {
                starts.push(i);
            }
            i = hdr;
        } else {
            i += 1;
        }
    }
    starts.push(data.len());
    starts
        .windows(2)
        .map(|w| data[w[0]..w[1]].to_vec())
        .collect()
}

fn is_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1f == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

fn ogg_opus_packets(data: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut i = 0;
    let mut cur = Vec::new();
    while i + 27 <= data.len() && &data[i..i + 4] == b"OggS" {
        let nseg = data[i + 26] as usize;
        let lacing = &data[i + 27..i + 27 + nseg];
        let mut off = i + 27 + nseg;
        for &l in lacing {
            cur.extend_from_slice(&data[off..off + l as usize]);
            off += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        i = off;
    }
    packets.into_iter().skip(2).collect() // OpusHead, OpusTags
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,str0m=warn,is=warn".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let media_dir = args.next().unwrap_or_else(|| "target/mock-media".into());
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8085);

    let video = std::fs::read(format!("{media_dir}/video.h264"))
        .expect("video.h264 (run scripts/mock-media.sh)");
    let aus = split_access_units(&video);
    let audio = std::fs::read(format!("{media_dir}/audio.opus")).expect("audio.opus");
    let opus = ogg_opus_packets(&audio);
    tracing::info!(
        frames = aus.len(),
        audio_packets = opus.len(),
        "media loaded"
    );

    let shared = Arc::new(Mutex::new(Shared::default()));
    {
        let shared = shared.clone();
        std::thread::spawn(move || http_server(port, shared));
    }
    tracing::info!("mock GSSV API on http://127.0.0.1:{port}");

    loop {
        // Wait for an offer.
        let offer = loop {
            if let Some(o) = shared.lock().unwrap().offer.take() {
                break o;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if let Err(e) = run_session(&shared, offer, &aus, &opus) {
            tracing::error!("session ended: {e}");
        }
        let mut s = shared.lock().unwrap();
        s.answer = None;
        s.remote_candidates.clear();
        s.local_candidate = None;
    }
}

fn run_session(
    shared: &Arc<Mutex<Shared>>,
    offer: String,
    aus: &[Vec<u8>],
    opus: &[Vec<u8>],
) -> Result<(), Box<dyn std::error::Error>> {
    let ip = xcloud::ice::local_addresses()
        .into_iter()
        .find(|ip| ip.is_ipv4())
        .ok_or("no local IPv4")?;
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0))?;
    let local = socket.local_addr()?;
    socket.set_read_timeout(Some(Duration::from_millis(2)))?;

    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_h264(true)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let answer = rtc
        .sdp_api()
        .accept_offer(SdpOffer::from_sdp_string(&offer)?)?;
    {
        let mut s = shared.lock().unwrap();
        s.answer = Some(answer.to_sdp_string());
        s.local_candidate = Some(format!(
            "a=candidate:1 1 UDP 100 {} {} typ host ",
            local.ip(),
            local.port()
        ));
    }
    tracing::info!(%local, "answer ready");

    let mut video_mid: Option<Mid> = None;
    let mut audio_mid: Option<Mid> = None;
    let mut channels: Vec<(ChannelId, String)> = Vec::new();
    let mut added_candidates = 0;
    let mut buf = vec![0u8; 2000];
    let start = Instant::now();
    let mut next_video = Instant::now();
    let mut next_audio = Instant::now();
    let mut frame_no: u64 = 0;
    let mut au_idx = 0usize;
    let mut audio_no: u64 = 0;
    let mut handshake = false;
    let mut last_buttons = 0u16;
    let mut input_packets = 0u64;
    let mut metadata_frames = 0u64;
    let mut last_report = Instant::now();
    let mut connected_at: Option<Instant> = None;

    loop {
        // Remote candidates from /ice.
        {
            let s = shared.lock().unwrap();
            for c in s.remote_candidates.iter().skip(added_candidates) {
                match Candidate::from_sdp_string(c.trim_start_matches("a=")) {
                    Ok(c) => rtc.add_remote_candidate(c),
                    Err(e) => tracing::warn!(%c, "bad candidate: {e}"),
                }
            }
            added_candidates = s.remote_candidates.len();
        }

        // Drain.
        let timeout = loop {
            match rtc.poll_output()? {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    socket.send_to(&t.contents, t.destination)?;
                }
                Output::Event(e) => match e {
                    Event::Connected => {
                        tracing::info!("client connected");
                        connected_at = Some(Instant::now());
                    }
                    Event::IceConnectionStateChange(s) => {
                        tracing::info!(?s, "ICE");
                        if s == str0m::IceConnectionState::Disconnected {
                            return Ok(());
                        }
                    }
                    Event::MediaAdded(m) => {
                        tracing::info!(mid = %m.mid, kind = ?m.kind, "media");
                        match m.kind {
                            MediaKind::Video => video_mid = Some(m.mid),
                            MediaKind::Audio => audio_mid = Some(m.mid),
                        }
                    }
                    Event::ChannelOpen(id, label) => {
                        tracing::info!(%label, "channel open");
                        channels.push((id, label));
                    }
                    Event::ChannelData(d) => {
                        let label = channels
                            .iter()
                            .find(|(id, _)| *id == d.id)
                            .map(|(_, l)| l.as_str())
                            .unwrap_or("?");
                        match label {
                            "message" => {
                                let v: Value = serde_json::from_slice(&d.data).unwrap_or_default();
                                match v["type"].as_str() {
                                    Some("Handshake") => {
                                        tracing::info!("message handshake → ack");
                                        let ack =
                                            json!({"type":"HandshakeAck","id":v["id"],"cv":"0"})
                                                .to_string();
                                        if let Some(mut ch) = rtc.channel(d.id) {
                                            ch.write(false, ack.as_bytes())?;
                                        }
                                        handshake = true;
                                        // Tell the client the stream size.
                                        if let Some((input, _)) =
                                            channels.iter().find(|(_, l)| l == "input")
                                            && let Some(mut ch) = rtc.channel(*input)
                                        {
                                            let mut p = vec![report::SERVER_METADATA as u8, 0];
                                            p.extend_from_slice(&1080u32.to_le_bytes());
                                            p.extend_from_slice(&1920u32.to_le_bytes());
                                            ch.write(true, &p)?;
                                        }
                                    }
                                    Some("Message") => {
                                        tracing::info!(target = %v["target"], "client message")
                                    }
                                    Some("TransactionComplete") => {
                                        tracing::info!(content = %v["content"], "dialog answered")
                                    }
                                    _ => tracing::info!(msg = %v, "message channel"),
                                }
                            }
                            "control" => {
                                let v: Value = serde_json::from_slice(&d.data).unwrap_or_default();
                                tracing::info!(msg = %v, "control");
                                if v["message"] == "videoKeyframeRequested" {
                                    // Jump to the next IDR.
                                    if let Some(i) = (0..aus.len())
                                        .map(|k| (au_idx + k) % aus.len())
                                        .find(|&k| is_idr(&aus[k]))
                                    {
                                        au_idx = i;
                                    }
                                }
                            }
                            "input" => {
                                input_packets += 1;
                                let p = &d.data;
                                let kind = u16::from_le_bytes([p[0], p[1]]);
                                let mut off = 14;
                                if kind & report::METADATA != 0 {
                                    let n = p[off] as usize;
                                    metadata_frames += n as u64;
                                    off += 1 + n * 28;
                                }
                                if kind & report::GAMEPAD != 0 && p.len() > off + 3 {
                                    let n = p[off] as usize;
                                    off += 1;
                                    for _ in 0..n {
                                        let idx = p[off];
                                        let buttons = u16::from_le_bytes([p[off + 1], p[off + 2]]);
                                        let lx = i16::from_le_bytes([p[off + 3], p[off + 4]]);
                                        if idx == 0 && buttons != last_buttons {
                                            tracing::info!(
                                                buttons = format!("{buttons:#06x}"),
                                                lx,
                                                "gamepad 0"
                                            );
                                            let pressed = buttons & !last_buttons;
                                            if pressed & button::A != 0 {
                                                // Rumble: 60% / 100%, 300 ms.
                                                let vib = [
                                                    128u8, 0, 0, 0, 60, 100, 0, 30, 0x2c, 0x01, 0,
                                                    0, 0,
                                                ];
                                                if let Some(mut ch) = rtc.channel(d.id) {
                                                    ch.write(true, &vib)?;
                                                }
                                            }
                                            if pressed & button::Y != 0
                                                && let Some((m, _)) =
                                                    channels.iter().find(|(_, l)| l == "message")
                                                && let Some(mut ch) = rtc.channel(*m)
                                            {
                                                let content = json!({"TitleText":"Quit game?","ContentText":"Progress since the last checkpoint will be lost.","CommandLabel1":"Quit","CommandLabel2":"Cancel"}).to_string();
                                                let msg = json!({"type":"TransactionStart","id":"dlg-1","target":"/streaming/systemUi/messages/ShowMessageDialog","content":content,"cv":""}).to_string();
                                                ch.write(false, msg.as_bytes())?;
                                            }
                                            last_buttons = buttons;
                                        }
                                        off += 23;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::KeyframeRequest(_) => {
                        tracing::info!("PLI received");
                        if let Some(i) = (0..aus.len())
                            .map(|k| (au_idx + k) % aus.len())
                            .find(|&k| is_idr(&aus[k]))
                        {
                            au_idx = i;
                        }
                    }
                    _ => {}
                },
            }
        };

        // Media pacing.
        let now = Instant::now();
        if handshake && connected_at.is_some() {
            if let Some(mid) = video_mid
                && now >= next_video
            {
                next_video += Duration::from_micros(16_667);
                if let Some(w) = rtc.writer(mid) {
                    let pt = w
                        .payload_params()
                        .find(|p| p.spec().codec == Codec::H264)
                        .map(|p| p.pt());
                    if let Some(pt) = pt {
                        let t = MediaTime::new(frame_no * 1500, Frequency::NINETY_KHZ);
                        w.write(pt, now, t, aus[au_idx].clone())?;
                        frame_no += 1;
                        au_idx = (au_idx + 1) % aus.len();
                    }
                }
                continue; // drain after mutation
            }
            if let Some(mid) = audio_mid
                && now >= next_audio
            {
                next_audio += Duration::from_millis(20);
                if let Some(w) = rtc.writer(mid) {
                    let pt = w
                        .payload_params()
                        .find(|p| p.spec().codec == Codec::Opus)
                        .map(|p| p.pt());
                    if let Some(pt) = pt {
                        let t = MediaTime::new(audio_no * 960, Frequency::FORTY_EIGHT_KHZ);
                        w.write(pt, now, t, opus[(audio_no as usize) % opus.len()].clone())?;
                        audio_no += 1;
                    }
                }
                continue;
            }
        }
        if last_report.elapsed() > Duration::from_secs(5) && handshake {
            last_report = Instant::now();
            tracing::info!(
                uptime = ?start.elapsed(),
                frames = frame_no,
                input_packets,
                metadata_frames,
                "stats"
            );
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .min(next_video.saturating_duration_since(Instant::now()))
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(wait))?;
        buf.resize(2000, 0);
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: buf[..n].try_into()?,
                },
            ),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Input::Timeout(Instant::now())
            }
            Err(e) => return Err(e.into()),
        };
        rtc.handle_input(input)?;
        if shared.lock().unwrap().offer.is_some() {
            tracing::info!("new offer, ending session");
            return Ok(());
        }
    }
}

fn respond(req: tiny_http::Request, status: u16, body: Option<Value>) {
    let resp = match body {
        Some(b) => tiny_http::Response::from_string(b.to_string())
            .with_status_code(status)
            .with_header(
                "Content-Type: application/json"
                    .parse::<tiny_http::Header>()
                    .unwrap(),
            ),
        None => tiny_http::Response::from_string("").with_status_code(status),
    };
    let _ = req.respond(resp);
}

fn http_server(port: u16, shared: Arc<Mutex<Shared>>) {
    let server = tiny_http::Server::http(("0.0.0.0", port)).expect("bind HTTP");
    for mut req in server.incoming_requests() {
        let method = req.method().to_string();
        let url = req.url().split('?').next().unwrap_or("").to_owned();
        let mut body = String::new();
        let _ = req.as_reader().read_to_string(&mut body);
        tracing::debug!(%method, %url, "HTTP");
        let parts: Vec<&str> = url.trim_matches('/').split('/').collect();
        match (method.as_str(), parts.as_slice()) {
            ("GET", ["v6", "servers", "home"]) => respond(
                req,
                200,
                Some(json!({"totalItems":2,"results":[
                    {"deviceName":"Mock Series X","serverId":"MOCK0001","powerState":"On","consoleType":"XboxSeriesX","playPath":"v5/sessions/home/play","outOfHomeWarning":false,"wirelessWarning":false,"isDevKit":false},
                    {"deviceName":"Bedroom","serverId":"MOCK0002","powerState":"ConnectedStandby","consoleType":"XboxSeriesS","playPath":"v5/sessions/home/play","outOfHomeWarning":false,"wirelessWarning":false,"isDevKit":false}
                ]})),
            ),
            ("GET", ["v2", "titles"]) | ("GET", ["v2", "titles", "mru"]) => {
                let names = [
                    "Forza Horizon",
                    "Halo Infinite",
                    "Starfield",
                    "Sea of Thieves",
                    "Hollow Knight",
                    "Ori",
                    "Gears 5",
                    "Hi-Fi Rush",
                ];
                let results: Vec<Value> = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| json!({"titleId": format!("MOCKTITLE{i}"), "details": {"productId": n, "hasEntitlement": true}}))
                    .collect();
                respond(req, 200, Some(json!({ "results": results })))
            }
            ("POST", ["v5", "sessions", kind, "play"]) => {
                let id = {
                    let mut s = shared.lock().unwrap();
                    s.session += 1;
                    s.polls = 0;
                    s.session
                };
                tracing::info!(%kind, body = %body, "play");
                respond(
                    req,
                    200,
                    Some(
                        json!({"sessionId": format!("S{id}"), "sessionPath": format!("v5/sessions/{kind}/S{id}"), "state": "Provisioning"}),
                    ),
                )
            }
            ("GET", ["v5", "sessions", _, _, "state"]) => {
                let polls = {
                    let mut s = shared.lock().unwrap();
                    s.polls += 1;
                    s.polls
                };
                let state = if polls < 2 {
                    "Provisioning"
                } else {
                    "Provisioned"
                };
                respond(req, 200, Some(json!({ "state": state })))
            }
            ("GET", ["v5", "sessions", _, _, "configuration"]) => {
                respond(req, 200, Some(json!({ "serverDetails": {} })))
            }
            ("POST", ["v5", "sessions", _, _, "sdp"]) => {
                let v: Value = serde_json::from_str(&body).unwrap_or_default();
                let sdp = v["sdp"].as_str().unwrap_or_default().to_owned();
                let mut s = shared.lock().unwrap();
                s.answer = None;
                s.offer = Some(sdp);
                drop(s);
                respond(req, 202, None)
            }
            ("GET", ["v5", "sessions", _, _, "sdp"]) => {
                let answer = shared.lock().unwrap().answer.clone();
                match answer {
                    Some(sdp) => {
                        let inner = json!({"sdp": sdp, "sdpType": "answer", "status": "success"})
                            .to_string();
                        respond(
                            req,
                            200,
                            Some(json!({"exchangeResponse": inner, "errorDetails": null})),
                        )
                    }
                    None => respond(req, 204, None),
                }
            }
            ("POST", ["v5", "sessions", _, _, "ice"]) => {
                let v: Value = serde_json::from_str(&body).unwrap_or_default();
                let mut s = shared.lock().unwrap();
                for c in v["candidate"].as_array().into_iter().flatten() {
                    if let Some(c) = c["candidate"].as_str() {
                        tracing::info!(candidate = c, "client candidate");
                        s.remote_candidates.push(c.to_owned());
                    }
                }
                drop(s);
                respond(req, 202, None)
            }
            ("GET", ["v5", "sessions", _, _, "ice"]) => {
                let c = shared.lock().unwrap().local_candidate.clone();
                match c {
                    Some(c) => {
                        let inner = json!([
                            {"candidate": c, "messageType": "iceCandidate", "sdpMLineIndex": "0", "sdpMid": "0"},
                            {"candidate": "a=end-of-candidates", "messageType": "iceCandidate", "sdpMLineIndex": "0", "sdpMid": "0"}
                        ])
                        .to_string();
                        respond(
                            req,
                            200,
                            Some(json!({"exchangeResponse": inner, "errorDetails": null})),
                        )
                    }
                    None => respond(req, 204, None),
                }
            }
            ("POST", ["v5", "sessions", _, _, "keepalive"]) => {
                respond(req, 200, Some(json!({"alertAfterIntervalInSeconds": 60})))
            }
            ("DELETE", ["v5", "sessions", _, _]) => {
                tracing::info!(%url, "session deleted");
                respond(req, 200, None)
            }
            ("GET", ["v1", "waittime", _]) => respond(req, 200, Some(json!({}))),
            _ => {
                tracing::warn!(%method, %url, "unhandled");
                respond(req, 404, None)
            }
        }
    }
}
