//! GSSV ("Game Streaming Service") REST API: console list, title list and
//! the streaming session lifecycle.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::check;
use crate::device_info::{Resolution, device_info};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    /// Remote play to the user's own console.
    Home,
    /// Cloud gaming.
    Cloud,
}

impl StreamKind {
    pub fn path(self) -> &'static str {
        match self {
            StreamKind::Home => "home",
            StreamKind::Cloud => "cloud",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    pub device_name: String,
    pub server_id: String,
    #[serde(default)]
    pub power_state: String,
    #[serde(default)]
    pub console_type: String,
    #[serde(default)]
    pub play_path: String,
    #[serde(default)]
    pub out_of_home_warning: bool,
    #[serde(default)]
    pub wireless_warning: bool,
    #[serde(default)]
    pub is_dev_kit: bool,
}

impl Console {
    pub fn is_on(&self) -> bool {
        self.power_state.eq_ignore_ascii_case("On")
    }

    pub fn pretty_type(&self) -> &str {
        match self.console_type.as_str() {
            "XboxSeriesX" => "Xbox Series X",
            "XboxSeriesS" => "Xbox Series S",
            "XboxOne" => "Xbox One",
            "XboxOneS" => "Xbox One S",
            "XboxOneX" => "Xbox One X",
            other => other,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConsolesResponse {
    #[serde(default)]
    results: Vec<Console>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TitleDetails {
    #[serde(default)]
    pub product_id: Option<String>,
    #[serde(default)]
    pub xbox_title_id: Option<serde_json::Value>,
    #[serde(default)]
    pub has_entitlement: bool,
    #[serde(default)]
    pub is_free_in_store: bool,
    #[serde(default)]
    pub blocked_by_family_safety: bool,
    #[serde(default)]
    pub supports_in_app_purchases: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Title {
    pub title_id: String,
    #[serde(default)]
    pub details: TitleDetails,
}

impl Title {
    pub fn playable(&self) -> bool {
        self.details.has_entitlement || self.details.is_free_in_store
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TitlesResponse {
    #[serde(default)]
    results: Vec<Title>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    #[serde(default)]
    pub session_id: String,
    pub session_path: String,
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetails {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl std::fmt::Display for ErrorDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            self.code.as_deref().unwrap_or("Unknown"),
            self.message.as_deref().unwrap_or("no details")
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub state: String,
    #[serde(default)]
    pub error_details: Option<ErrorDetails>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WaitTime {
    #[serde(default)]
    pub estimated_total_wait_time_in_seconds: Option<i64>,
    #[serde(default)]
    pub estimated_allocation_time_in_seconds: Option<i64>,
    #[serde(default)]
    pub estimated_provisioning_time_in_seconds: Option<i64>,
}

/// Console network details returned by `/configuration` for remote play.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerDetails {
    #[serde(default)]
    pub ip_address: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default, rename = "ipV4Address")]
    pub ipv4_address: Option<String>,
    #[serde(default, rename = "ipV4Port")]
    pub ipv4_port: Option<u16>,
    #[serde(default, rename = "ipV6Address")]
    pub ipv6_address: Option<String>,
    #[serde(default, rename = "ipV6Port")]
    pub ipv6_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionConfiguration {
    #[serde(default)]
    server_details: Option<ServerDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeResponse {
    #[serde(default)]
    exchange_response: Option<String>,
    #[serde(default)]
    error_details: Option<ErrorDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SdpAnswer {
    #[serde(default)]
    sdp: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    input: Option<u8>,
    #[serde(default)]
    control: Option<u8>,
    #[serde(default)]
    message: Option<u8>,
}

/// Result of the SDP exchange.
#[derive(Debug, Clone)]
pub struct SdpExchange {
    pub sdp: String,
    /// Negotiated input channel protocol version (if reported).
    pub input_version: Option<u8>,
    pub control_version: Option<u8>,
    pub message_version: Option<u8>,
}

/// An ICE candidate as exchanged with the GSSV `/ice` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidateJson {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex", default, deserialize_with = "num_or_string")]
    pub sdp_mline_index: Option<u32>,
}

fn num_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n as u32),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeepaliveResponse {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub alert_after_interval_in_seconds: Option<u64>,
}

/// GSSV API client bound to one region base URI and streaming token.
#[derive(Clone)]
pub struct GssvApi {
    http: reqwest::Client,
    base: String,
    token: String,
    kind: StreamKind,
    resolution: Resolution,
    locale: String,
}

impl GssvApi {
    pub fn new(
        http: reqwest::Client,
        base_uri: &str,
        gs_token: &str,
        kind: StreamKind,
        resolution: Resolution,
        locale: &str,
    ) -> Self {
        Self {
            http,
            base: base_uri.trim_end_matches('/').to_owned(),
            token: gs_token.to_owned(),
            kind,
            resolution,
            locale: locale.to_owned(),
        }
    }

    pub fn kind(&self) -> StreamKind {
        self.kind
    }

    pub fn base_uri(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .header("X-Gssv-Client", "XboxComBrowser")
            .header("X-MS-Device-Info", device_info(self.resolution))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let resp = self.request(reqwest::Method::GET, path).send().await?;
        Ok(check(resp).await?.json().await?)
    }

    /// GET returning `None` while the server answers 204 (not ready yet).
    async fn get_maybe<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Option<T>> {
        let resp = self.request(reqwest::Method::GET, path).send().await?;
        let resp = check(resp).await?;
        if resp.status().as_u16() == 204 {
            return Ok(None);
        }
        let text = resp.text().await?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> Result<String> {
        let resp = self
            .request(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await?;
        Ok(check(resp).await?.text().await?)
    }

    pub async fn consoles(&self) -> Result<Vec<Console>> {
        let r: ConsolesResponse = self.get_json("/v6/servers/home?mr=50").await?;
        Ok(r.results)
    }

    pub async fn titles(&self) -> Result<Vec<Title>> {
        let r: TitlesResponse = self.get_json("/v2/titles").await?;
        Ok(r.results)
    }

    pub async fn recent_titles(&self) -> Result<Vec<Title>> {
        let r: TitlesResponse = self.get_json("/v2/titles/mru?mr=25").await?;
        Ok(r.results)
    }

    pub async fn wait_time(&self, title_id: &str) -> Result<WaitTime> {
        self.get_json(&format!("/v1/waittime/{title_id}")).await
    }

    /// Start a session. `target` is the console `serverId` for home
    /// streaming or the `titleId` for cloud streaming.
    pub async fn start_session(&self, target: &str) -> Result<SessionInfo> {
        // Cloud: patch both header + body like the official client does when
        // it detects a TV / Windows device. Home: only the header is used.
        let os_name = match self.kind {
            StreamKind::Cloud => self.resolution.os_name(),
            StreamKind::Home => "windows",
        };
        let tz_offset = chrono::Local::now().offset().local_minus_utc() / 60;
        let body = json!({
            "clientSessionId": "",
            "titleId": if self.kind == StreamKind::Cloud { target } else { "" },
            "systemUpdateGroup": "",
            "settings": {
                "nanoVersion": "V3;WebrtcTransport.dll",
                "enableOptionalDataCollection": false,
                "enableTextToSpeech": false,
                "highContrast": 0,
                "locale": self.locale,
                "useIceConnection": false,
                "timezoneOffsetMinutes": tz_offset,
                "sdkType": "web",
                "osName": os_name,
            },
            "serverId": if self.kind == StreamKind::Home { target } else { "" },
            "fallbackRegionNames": [],
        });
        let text = self
            .post(&format!("/v5/sessions/{}/play", self.kind.path()), body)
            .await?;
        let mut info: SessionInfo = serde_json::from_str(&text)?;
        if info.session_id.is_empty() {
            info.session_id = info
                .session_path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
        }
        Ok(info)
    }

    pub async fn session_state(&self, s: &SessionInfo) -> Result<SessionState> {
        self.get_json(&format!("/{}/state", s.session_path)).await
    }

    pub async fn connect(&self, s: &SessionInfo, transfer_token: &str) -> Result<()> {
        self.post(
            &format!("/{}/connect", s.session_path),
            json!({ "userToken": transfer_token }),
        )
        .await?;
        Ok(())
    }

    pub async fn server_details(&self, s: &SessionInfo) -> Result<Option<ServerDetails>> {
        let c: Option<SessionConfiguration> = self
            .get_maybe(&format!("/{}/configuration", s.session_path))
            .await?;
        Ok(c.and_then(|c| c.server_details))
    }

    /// Send our SDP offer and wait for the answer.
    pub async fn exchange_sdp(&self, s: &SessionInfo, offer_sdp: &str) -> Result<SdpExchange> {
        let body = json!({
            "messageType": "offer",
            "sdp": offer_sdp,
            "requestId": "1",
            "configuration": {
                "chatConfiguration": {
                    "bytesPerSample": 2,
                    "expectedClipDurationMs": 20,
                    "format": { "codec": "opus", "container": "webm" },
                    "numChannels": 1,
                    "sampleFrequencyHz": 24000,
                },
                "chat": { "minVersion": 1, "maxVersion": 1 },
                "control": { "minVersion": 1, "maxVersion": 3 },
                "input": { "minVersion": 1, "maxVersion": 9 },
                "message": { "minVersion": 1, "maxVersion": 1 },
                "reliableinput": { "minVersion": 9, "maxVersion": 9 },
                "unreliableinput": { "minVersion": 9, "maxVersion": 9 },
            },
        });
        let path = format!("/{}/sdp", s.session_path);
        self.post(&path, body).await?;

        let answer: ExchangeResponse = self.poll_exchange(&path).await?;
        let raw = answer.exchange_response.ok_or_else(|| {
            Error::Session(format!(
                "SDP exchange failed: {}",
                answer.error_details.unwrap_or_default()
            ))
        })?;
        let parsed: SdpAnswer = serde_json::from_str(&raw)?;
        tracing::info!(
            input = ?parsed.input,
            control = ?parsed.control,
            message = ?parsed.message,
            "negotiated channel versions"
        );
        match (parsed.status.as_deref(), parsed.sdp) {
            (Some("success") | None, Some(sdp)) => Ok(SdpExchange {
                sdp,
                input_version: parsed.input,
                control_version: parsed.control,
                message_version: parsed.message,
            }),
            (status, _) => Err(Error::Session(format!(
                "server rejected our SDP offer (status {status:?})"
            ))),
        }
    }

    /// Send our ICE candidates and wait for the server's.
    pub async fn exchange_ice(
        &self,
        s: &SessionInfo,
        candidates: &[IceCandidateJson],
    ) -> Result<Vec<IceCandidateJson>> {
        let path = format!("/{}/ice", s.session_path);
        self.post(
            &path,
            json!({ "messageType": "iceCandidate", "candidate": candidates }),
        )
        .await?;
        let resp: ExchangeResponse = self.poll_exchange(&path).await?;
        let raw = resp.exchange_response.ok_or_else(|| {
            Error::Session(format!(
                "ICE exchange failed: {}",
                resp.error_details.unwrap_or_default()
            ))
        })?;
        Ok(serde_json::from_str(&raw)?)
    }

    async fn poll_exchange(&self, path: &str) -> Result<ExchangeResponse> {
        let mut errors = 0;
        for _ in 0..120 {
            match self.get_maybe::<ExchangeResponse>(path).await {
                Ok(Some(r)) => return Ok(r),
                Ok(None) => {}
                Err(e) if is_transient(&e) && errors < 5 => {
                    errors += 1;
                    tracing::warn!("retrying {path}: {e}");
                }
                Err(e) => return Err(e),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(Error::Session(format!("timed out waiting for {path}")))
    }

    pub async fn keepalive(&self, s: &SessionInfo) -> Result<KeepaliveResponse> {
        let text = self
            .post(&format!("/{}/keepalive", s.session_path), json!({}))
            .await?;
        if text.trim().is_empty() {
            return Ok(KeepaliveResponse::default());
        }
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub async fn stop(&self, s: &SessionInfo) -> Result<()> {
        let resp = self
            .request(reqwest::Method::DELETE, &format!("/{}", s.session_path))
            .send()
            .await?;
        check(resp).await?;
        Ok(())
    }
}

/// Network hiccups and 5xx responses are worth retrying; 4xx are not.
pub fn is_transient(e: &Error) -> bool {
    match e {
        Error::Network(_) => true,
        Error::Http { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ice_response() {
        let raw = r#"[{"candidate":"a=candidate:1 1 UDP 100 198.51.100.10 1136 typ host ","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"},{"candidate":"a=end-of-candidates","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"}]"#;
        let c: Vec<IceCandidateJson> = serde_json::from_str(raw).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].sdp_mline_index, Some(0));
    }

    #[test]
    fn parses_consoles() {
        let raw = r#"{"totalItems":1,"results":[{"deviceName":"XboxSeriesX","serverId":"F400000000000000","powerState":"ConnectedStandby","consoleType":"XboxSeriesX","playPath":"v5/sessions/home/play","outOfHomeWarning":true,"wirelessWarning":false,"isDevKit":false}],"continuationToken":null}"#;
        let r: ConsolesResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.results[0].server_id, "F400000000000000");
        assert_eq!(r.results[0].pretty_type(), "Xbox Series X");
    }
}
