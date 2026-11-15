//! Session orchestration: provisioning, SDP/ICE signalling over the GSSV
//! REST API, and handing the connected `Rtc` to the network thread.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use str0m::change::SdpAnswer;
use str0m::channel::{ChannelConfig, Reliability};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, MediaKind};
use str0m::{Candidate, Rtc};

use super::rtc::{self, ChannelIds, DriverInit};
use super::{Command, StreamHandle, StreamSinks};
use crate::account::Account;
use crate::api::{GssvApi, IceCandidateJson, SessionInfo, StreamKind};
use crate::device_info::Resolution;
use crate::error::{Error, Result};
use crate::ice;

/// H.264 profile preference sent in our offer. The server encodes with the
/// first profile it supports, in our order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CodecPreference {
    /// High → Main → Baseline. High profile is not advertised by the official
    /// web client; the server falls back to Main if it doesn't support it.
    High,
    /// Main → Baseline (CABAC, best quality the official client gets).
    #[default]
    Main,
    /// Constrained baseline only (lowest decode cost).
    Baseline,
}

impl CodecPreference {
    pub const ALL: [CodecPreference; 3] = [Self::High, Self::Main, Self::Baseline];

    pub fn label(self) -> &'static str {
        match self {
            Self::High => "High (experimental)",
            Self::Main => "Main",
            Self::Baseline => "Baseline",
        }
    }

    /// (payload type, rtx payload type, profile-level-id)
    fn h264_params(self) -> Vec<(u8, u8, u32)> {
        let high = (102, 103, 0x64001f);
        let main = (123, 119, 0x4d001f);
        let cb = (108, 109, 0x42e01f);
        let base = (127, 121, 0x42001f);
        match self {
            Self::High => vec![high, main, cb, base],
            Self::Main => vec![main, cb, base],
            Self::Baseline => vec![cb, base],
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub resolution: Resolution,
    pub codec: CodecPreference,
    /// Optional receive bitrate cap (kbps) signalled with `b=AS`.
    pub max_video_kbps: Option<u32>,
    pub locale: String,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::default(),
            codec: CodecPreference::default(),
            max_video_kbps: None,
            locale: "en-US".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionProgress {
    Requesting,
    Created(SessionInfo),
    Queued { wait_seconds: Option<i64> },
    Provisioning,
    Authorizing,
    Negotiating,
    Connecting,
}

/// A running session. Dropping it does not stop the server session; call
/// [`StreamSession::stop`].
pub struct StreamSession {
    pub handle: StreamHandle,
    pub info: SessionInfo,
    api: GssvApi,
    keepalive: tokio::task::JoinHandle<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StreamSession {
    pub fn api(&self) -> &GssvApi {
        &self.api
    }

    pub async fn stop(mut self) {
        self.handle.send(Command::Shutdown);
        self.keepalive.abort();
        if let Some(t) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || t.join()).await;
        }
        if let Err(e) = self.api.stop(&self.info).await {
            tracing::warn!("failed to stop session: {e}");
        }
    }
}

fn build_rtc(cfg: &StreamConfig) -> Rtc {
    let mut builder = Rtc::builder()
        .clear_codecs()
        // A 1080p keyframe at high bitrate easily spans several hundred
        // RTP packets; keep enough room to reassemble it.
        .set_reordering_size_video(4096)
        .set_reordering_size_audio(8)
        .set_stats_interval(Some(Duration::from_secs(1)));
    {
        let codecs = builder.codec_config();
        codecs.add_config(
            111.into(),
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                min_p_time: Some(10),
                use_inband_fec: Some(true),
                stereo: Some(true),
                ..Default::default()
            },
        );
        for (pt, rtx, plid) in cfg.codec.h264_params() {
            codecs.add_h264(pt.into(), Some(rtx.into()), true, plid);
        }
    }
    builder.build(Instant::now())
}

/// Insert / replace `b=AS:<kbps>` in the video m-section.
pub(crate) fn apply_video_bitrate(sdp: &str, kbps: u32) -> String {
    let mut out = Vec::new();
    let mut in_video = false;
    let mut inserted = false;
    for line in sdp.split("\r\n") {
        if line.starts_with("m=") {
            in_video = line.starts_with("m=video");
            out.push(line.to_owned());
            continue;
        }
        if in_video && line.starts_with("b=AS:") {
            continue;
        }
        out.push(line.to_owned());
        if in_video && !inserted && line.starts_with("c=") {
            out.push(format!("b=AS:{kbps}"));
            inserted = true;
        }
    }
    out.join("\r\n")
}

fn channel(label: &str, protocol: &str) -> ChannelConfig {
    ChannelConfig {
        label: label.into(),
        ordered: true,
        reliability: Reliability::Reliable,
        negotiated: None,
        protocol: protocol.into(),
    }
}

/// Start a stream end to end. `target` is a console `serverId` (home) or an
/// xCloud `titleId` (cloud).
pub async fn start_stream(
    account: Option<Arc<Account>>,
    api: GssvApi,
    target: String,
    cfg: StreamConfig,
    sinks: StreamSinks,
    progress: impl Fn(SessionProgress) + Send + Sync + 'static,
) -> Result<StreamSession> {
    progress(SessionProgress::Requesting);
    let info = api.start_session(&target).await?;
    tracing::info!(session = %info.session_id, "session created");
    progress(SessionProgress::Created(info.clone()));

    // 1. Wait for provisioning.
    let mut connect_sent_at: Option<Instant> = None;
    let mut wait_fetched = false;
    let mut errors = 0;
    let started = Instant::now();
    loop {
        let state = match api.session_state(&info).await {
            Ok(s) => {
                errors = 0;
                s
            }
            Err(e) if crate::api::is_transient(&e) && errors < 10 => {
                errors += 1;
                tracing::warn!("session state poll failed (will retry): {e}");
                tokio::time::sleep(Duration::from_millis(1000)).await;
                continue;
            }
            Err(Error::Http { status: 404, .. }) => {
                return Err(Error::Session("The session no longer exists".into()));
            }
            Err(e) => return Err(e),
        };
        tracing::debug!(state = %state.state, "session state");
        match state.state.as_str() {
            "Provisioned" => break,
            "Provisioning" => progress(SessionProgress::Provisioning),
            "WaitingForResources" => {
                if !wait_fetched && api.kind() == StreamKind::Cloud {
                    wait_fetched = true;
                    let w = api.wait_time(&target).await.unwrap_or_default();
                    progress(SessionProgress::Queued {
                        wait_seconds: w.estimated_total_wait_time_in_seconds,
                    });
                }
            }
            "ReadyToConnect" => {
                // Re-send if the server still waits for us after a while.
                if connect_sent_at.is_none_or(|t| t.elapsed() > Duration::from_secs(8)) {
                    connect_sent_at = Some(Instant::now());
                    progress(SessionProgress::Authorizing);
                    let account = account
                        .as_ref()
                        .ok_or_else(|| Error::Session("Cloud sessions require sign-in".into()))?;
                    let token = account.transfer_token().await?;
                    api.connect(&info, &token).await?;
                }
            }
            "Failed" => {
                let details = state.error_details.unwrap_or_default();
                let msg = match details.code.as_deref() {
                    Some("WNSError")
                        if details
                            .message
                            .as_deref()
                            .is_some_and(|m| m.contains("WaitingForServerToRegister")) =>
                    {
                        "The console is not reachable. Make sure it's on (or in sleep mode) and that remote features are enabled.".to_owned()
                    }
                    _ => format!("Failed to start the session ({details})"),
                };
                return Err(Error::Session(msg));
            }
            other => tracing::debug!(state = other, "unknown session state"),
        }
        if started.elapsed() > Duration::from_secs(60 * 30) {
            return Err(Error::Session("Timed out waiting for the session".into()));
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    // 2. Build the peer connection + offer.
    progress(SessionProgress::Negotiating);
    let server_details = if api.kind() == StreamKind::Home {
        api.server_details(&info).await.unwrap_or_else(|e| {
            tracing::warn!("no console configuration: {e}");
            None
        })
    } else {
        None
    };

    let mut rtc = build_rtc(&cfg);
    let mut pending_events = Vec::new();
    let mut sdp = rtc.sdp_api();
    let audio_mid = sdp.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let video_mid = sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    let chat = sdp.add_channel_with_config(channel("chat", "chatV1"));
    let control = sdp.add_channel_with_config(channel("control", "controlV1"));
    let input = sdp.add_channel_with_config(channel("input", "1.0"));
    let message = sdp.add_channel_with_config(channel("message", "messageV1"));
    let (offer, pending) = sdp
        .apply()
        .ok_or_else(|| Error::Rtc("failed to create offer".into()))?;
    rtc::drain_setup(&mut rtc, &mut pending_events)?;

    let mut offer_sdp = offer.to_sdp_string();
    if let Some(kbps) = cfg.max_video_kbps {
        offer_sdp = apply_video_bitrate(&offer_sdp, kbps);
    }
    tracing::debug!(sdp = %offer_sdp, "local offer");

    let exchange = api.exchange_sdp(&info, &offer_sdp).await?;
    let answer_sdp = exchange.sdp;
    let input_version = exchange.input_version.unwrap_or(9);
    tracing::debug!(sdp = %answer_sdp, "remote answer");
    let answer = SdpAnswer::from_sdp_string(&answer_sdp)
        .map_err(|e| Error::Rtc(format!("bad SDP answer: {e}")))?;
    rtc.sdp_api().accept_answer(pending, answer)?;
    rtc::drain_setup(&mut rtc, &mut pending_events)?;

    // 3. ICE: bind sockets, exchange candidates.
    progress(SessionProgress::Connecting);
    let locals = ice::local_addresses();
    let sockets = rtc::bind_sockets(&locals);
    if sockets.is_empty() {
        return Err(Error::Session("No usable network interface".into()));
    }
    let mut local_json = Vec::new();
    for s in &sockets {
        let c = Candidate::host(s.local, "udp").map_err(|e| Error::Rtc(e.to_string()))?;
        if let Some(added) = rtc.add_local_candidate(c) {
            local_json.push(IceCandidateJson {
                candidate: added.to_sdp_string(),
                sdp_mid: Some(audio_mid.to_string()),
                sdp_mline_index: Some(0),
            });
        }
        rtc::drain_setup(&mut rtc, &mut pending_events)?;
    }
    // Server-reflexive candidates: lets a console behind another NAT reach us.
    for (mapped, base) in rtc::stun_mappings(&sockets, Duration::from_millis(400)).await {
        if mapped == base {
            continue; // not behind NAT
        }
        tracing::info!(%mapped, %base, "public address (STUN)");
        if let Ok(c) = Candidate::server_reflexive(mapped, base, "udp")
            && let Some(added) = rtc.add_local_candidate(c)
        {
            local_json.push(IceCandidateJson {
                candidate: added.to_sdp_string(),
                sdp_mid: Some(audio_mid.to_string()),
                sdp_mline_index: Some(0),
            });
        }
        rtc::drain_setup(&mut rtc, &mut pending_events)?;
    }
    let remote_json = api.exchange_ice(&info, &local_json).await?;
    let has_v6 = sockets.iter().any(|s| s.local.is_ipv6());
    let remote: Vec<Candidate> = ice::remote_candidates(&remote_json, server_details.as_ref())
        .into_iter()
        .filter(|c| has_v6 || c.addr.is_ipv4())
        .filter_map(|c| Candidate::host(c.addr, "udp").ok())
        .collect();
    if remote.is_empty() {
        return Err(Error::Session("The server sent no usable ICE candidates".into()));
    }

    // 4. Hand everything to the network thread.
    let (w, h) = cfg.resolution.dimensions();
    let (handle, thread) = rtc::spawn(DriverInit {
        rtc,
        sockets,
        remote_candidates: remote,
        channels: ChannelIds {
            chat,
            control,
            input,
            message,
        },
        audio_mid,
        video_mid,
        pending_events,
        sinks,
        video_size: (w, h),
        input_version,
    })?;

    // 5. Keepalive.
    let keepalive = {
        let api = api.clone();
        let info = info.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                match api.keepalive(&info).await {
                    Ok(r) => {
                        if matches!(
                            r.code.as_deref(),
                            Some("SessionNotActive" | "SessionNotFound")
                        ) {
                            tracing::info!("session no longer active, stopping keepalive");
                            break;
                        }
                    }
                    Err(crate::Error::Http { status: 401 | 403 | 404, .. }) => {
                        tracing::warn!("keepalive rejected; session token expired or session gone");
                        break;
                    }
                    Err(e) => tracing::warn!("keepalive failed: {e}"),
                }
            }
        })
    };

    Ok(StreamSession {
        handle,
        info,
        api,
        keepalive,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_munging() {
        let sdp = "v=0\r\nm=audio 9 UDP 111\r\nc=IN IP4 0.0.0.0\r\nm=video 9 UDP 96\r\nc=IN IP4 0.0.0.0\r\na=mid:1\r\n";
        let out = apply_video_bitrate(sdp, 20000);
        assert!(out.contains("m=video 9 UDP 96\r\nc=IN IP4 0.0.0.0\r\nb=AS:20000\r\na=mid:1"));
        assert_eq!(out.matches("b=AS").count(), 1);
    }

    /// Feed a real xCloud answer (captured from the official client, mids
    /// rewritten to ours) to make sure str0m negotiates H.264 + data channels.
    #[test]
    fn accepts_xcloud_answer() {
        let mut rtc = build_rtc(&StreamConfig::default());
        let mut sdp = rtc.sdp_api();
        let a = sdp.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let v = sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let _ = sdp.add_channel_with_config(channel("chat", "chatV1"));
        let (offer, pending) = sdp.apply().unwrap();
        let offer = offer.to_sdp_string();
        assert!(offer.contains("profile-level-id=4d001f"), "{offer}");
        assert!(offer.contains("stereo=1"), "{offer}");
        // The application mid is the third one.
        let app_mid = offer
            .lines()
            .filter_map(|l| l.strip_prefix("a=mid:"))
            .nth(2)
            .unwrap()
            .to_owned();

        let answer = include_str!("../../tests/fixtures/xcloud_answer.sdp")
            .replace("{A}", &a.to_string())
            .replace("{V}", &v.to_string())
            .replace("{D}", &app_mid)
            .replace('\n', "\r\n");
        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        rtc.sdp_api().accept_answer(pending, answer).unwrap();
        let mut ev = Vec::new();
        rtc::drain_setup(&mut rtc, &mut ev).unwrap();
        let media = rtc.media(v).expect("video media");
        assert_eq!(media.kind(), MediaKind::Video);
        let pts: Vec<_> = rtc
            .codec_config()
            .params()
            .iter()
            .filter(|p| p.spec().codec == Codec::H264)
            .map(|p| p.pt())
            .collect();
        assert!(!pts.is_empty());
    }
}
