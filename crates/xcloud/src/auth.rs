//! Microsoft account + Xbox Live authentication.
//!
//! Flow (same as xbox.com / Greenlight, but driven natively):
//!
//! 1. MSAL device-code login against the consumer tenant using the
//!    xbox.com public client id. Works great on a Steam Deck: the user scans
//!    a QR code / types a short code on their phone.
//! 2. MSA access token -> Xbox Live user token (`user.auth.xboxlive.com`).
//! 3. User token -> XSTS token for the relying party we need
//!    (`http://gssv.xboxlive.com/` for streaming, `http://xboxlive.com` for
//!    profile data).
//! 4. XSTS token -> GSSV streaming token (`/v2/login/user`) for the `xhome`
//!    (remote play) or `xgpuweb` (cloud gaming) offering.
//! 5. For cloud sessions the server additionally asks for a "transfer token"
//!    (MSA refresh-token exchange) once the session is `ReadyToConnect`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};

/// Public client id used by xbox.com (MSAL).
pub const MSAL_CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const MSAL_SCOPE: &str = "xboxlive.signin openid profile offline_access";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const LIVE_TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
const TRANSFER_SCOPE: &str =
    "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

pub const RP_GSSV: &str = "http://gssv.xboxlive.com/";
pub const RP_XBOXLIVE: &str = "http://xboxlive.com";

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

impl DeviceCode {
    /// URL that pre-fills the code; handy for QR codes.
    pub fn verification_uri_complete(&self) -> String {
        format!("https://www.microsoft.com/link?otc={}", self.user_code)
    }
}

/// Persisted Microsoft account token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsaToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds).
    pub expires_at: i64,
}

impl MsaToken {
    pub fn is_fresh(&self) -> bool {
        Utc::now().timestamp() + 120 < self.expires_at
    }
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub enum DevicePoll {
    Pending,
    SlowDown,
    Complete(MsaToken),
}

/// An Xbox Live user or XSTS token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XblToken {
    pub token: String,
    pub not_after: DateTime<Utc>,
    pub uhs: String,
    pub xuid: Option<String>,
    pub gamertag: Option<String>,
}

impl XblToken {
    pub fn is_fresh(&self) -> bool {
        Utc::now() + chrono::Duration::seconds(120) < self.not_after
    }

    /// `Authorization` header value for Xbox Live web APIs.
    pub fn authorization(&self) -> String {
        format!("XBL3.0 x={};{}", self.uhs, self.token)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XblResponse {
    token: String,
    not_after: DateTime<Utc>,
    display_claims: XblDisplayClaims,
}

#[derive(Debug, Deserialize)]
struct XblDisplayClaims {
    xui: Vec<XblUserInfo>,
}

#[derive(Debug, Deserialize)]
struct XblUserInfo {
    #[serde(default)]
    uhs: Option<String>,
    #[serde(default)]
    xid: Option<String>,
    #[serde(default)]
    gtg: Option<String>,
}

impl From<XblResponse> for XblToken {
    fn from(r: XblResponse) -> Self {
        let first = r.display_claims.xui.into_iter().next();
        let (uhs, xuid, gamertag) = match first {
            Some(u) => (u.uhs.unwrap_or_default(), u.xid, u.gtg),
            None => (String::new(), None, None),
        };
        XblToken {
            token: r.token,
            not_after: r.not_after,
            uhs,
            xuid,
            gamertag,
        }
    }
}

/// A GSSV streaming token (`gsToken`) plus the region list for an offering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamToken {
    pub gs_token: String,
    #[serde(default)]
    pub market: String,
    pub duration_in_seconds: i64,
    pub offering_settings: OfferingSettings,
    /// Local bookkeeping (not part of the server response).
    #[serde(default)]
    pub obtained_at: i64,
}

impl StreamToken {
    pub fn is_fresh(&self) -> bool {
        Utc::now().timestamp() + 300 < self.obtained_at + self.duration_in_seconds
    }

    pub fn default_region(&self) -> Option<&Region> {
        self.offering_settings
            .regions
            .iter()
            .find(|r| r.is_default)
            .or_else(|| self.offering_settings.regions.first())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OfferingSettings {
    #[serde(default)]
    pub allow_region_selection: bool,
    #[serde(default)]
    pub regions: Vec<Region>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub name: String,
    pub base_uri: String,
    #[serde(default)]
    pub network_test_hostname: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub fallback_priority: i64,
}

/// Which streaming product a GSSV token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Offering {
    /// Remote play to the user's own console.
    XHome,
    /// Game Pass Ultimate cloud gaming.
    XCloud,
    /// Free-to-play cloud gaming (no Game Pass required).
    XCloudF2p,
}

impl Offering {
    pub fn id(self) -> &'static str {
        match self {
            Offering::XHome => "xhome",
            Offering::XCloud => "xgpuweb",
            Offering::XCloudF2p => "xgpuwebf2p",
        }
    }
}

/// Stateless authentication calls. See [`crate::account::Account`] for the
/// caching/refreshing layer.
#[derive(Clone)]
pub struct Auth {
    http: reqwest::Client,
}

impl Auth {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn start_device_code(&self) -> Result<DeviceCode> {
        let resp = self
            .http
            .post(DEVICE_CODE_URL)
            .form(&[("client_id", MSAL_CLIENT_ID), ("scope", MSAL_SCOPE)])
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    /// Poll once. Callers should respect `DeviceCode::interval`.
    pub async fn poll_device_code(&self, code: &DeviceCode) -> Result<DevicePoll> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", MSAL_CLIENT_ID),
                ("device_code", code.device_code.as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if status.is_success() {
            let t: OAuthTokenResponse = serde_json::from_str(&body)?;
            return Ok(DevicePoll::Complete(msa_from_response(t, None)?));
        }
        let err: OAuthErrorResponse = serde_json::from_str(&body).map_err(|_| Error::Http {
            status: status.as_u16(),
            body: body.clone(),
        })?;
        match err.error.as_str() {
            "authorization_pending" => Ok(DevicePoll::Pending),
            "slow_down" => Ok(DevicePoll::SlowDown),
            "expired_token" => Err(Error::Auth("The login code expired, please retry.".into())),
            "authorization_declined" => Err(Error::Auth("Login was declined.".into())),
            other => Err(Error::Auth(format!(
                "{other}: {}",
                err.error_description.unwrap_or_default()
            ))),
        }
    }

    /// Full device-code login, polling until done or expired.
    pub async fn device_code_login(&self, code: &DeviceCode) -> Result<MsaToken> {
        let mut interval = Duration::from_secs(code.interval.max(1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in);
        loop {
            tokio::time::sleep(interval).await;
            if tokio::time::Instant::now() > deadline {
                return Err(Error::Auth("The login code expired, please retry.".into()));
            }
            match self.poll_device_code(code).await? {
                DevicePoll::Pending => {}
                DevicePoll::SlowDown => interval += Duration::from_secs(5),
                DevicePoll::Complete(t) => return Ok(t),
            }
        }
    }

    pub async fn refresh_msa(&self, refresh_token: &str) -> Result<MsaToken> {
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", MSAL_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", MSAL_SCOPE),
            ])
            .send()
            .await?;
        if resp.status().as_u16() == 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::ReauthRequired(body));
        }
        let t: OAuthTokenResponse = check(resp).await?.json().await?;
        msa_from_response(t, Some(refresh_token))
    }

    pub async fn xbl_user_token(&self, msa_access_token: &str) -> Result<XblToken> {
        let body = json!({
            "Properties": {
                "AuthMethod": "RPS",
                "RpsTicket": format!("d={msa_access_token}"),
                "SiteName": "user.auth.xboxlive.com",
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        });
        let resp = self
            .http
            .post("https://user.auth.xboxlive.com/user/authenticate")
            .header("x-xbl-contract-version", "1")
            .header("Origin", "https://www.xbox.com")
            .header("Referer", "https://www.xbox.com/")
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await?;
        let r: XblResponse = check(resp).await?.json().await?;
        Ok(r.into())
    }

    pub async fn xsts_token(&self, user_token: &str, relying_party: &str) -> Result<XblToken> {
        let body = json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [user_token],
            },
            "RelyingParty": relying_party,
            "TokenType": "JWT",
        });
        let resp = self
            .http
            .post("https://xsts.auth.xboxlive.com/xsts/authorize")
            .header("x-xbl-contract-version", "1")
            .header("Origin", "https://www.xbox.com")
            .header("Referer", "https://www.xbox.com/")
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await?;
        if resp.status().as_u16() == 401 {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Auth(xsts_error_message(&body)));
        }
        let r: XblResponse = check(resp).await?.json().await?;
        Ok(r.into())
    }

    pub async fn gssv_login(&self, offering: Offering, gssv_xsts: &str) -> Result<StreamToken> {
        let url = format!(
            "https://{}.gssv-play-prod.xboxlive.com/v2/login/user",
            offering.id()
        );
        let resp = self
            .http
            .post(url)
            .header("x-gssv-client", "XboxComBrowser")
            .header("Cache-Control", "no-store, must-revalidate, no-cache")
            .json(&json!({ "token": gssv_xsts, "offeringId": offering.id() }))
            .send()
            .await?;
        let mut t: StreamToken = check(resp).await?.json().await?;
        t.obtained_at = Utc::now().timestamp();
        Ok(t)
    }

    /// Exchange the MSA refresh token for a cloud console transfer token
    /// (required by `/connect` on xCloud sessions).
    pub async fn transfer_token(&self, refresh_token: &str) -> Result<String> {
        let resp = self
            .http
            .post(LIVE_TOKEN_URL)
            .form(&[
                ("client_id", MSAL_CLIENT_ID),
                ("scope", TRANSFER_SCOPE),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await?;
        let t: OAuthTokenResponse = check(resp).await?.json().await?;
        Ok(t.access_token)
    }
}

fn msa_from_response(t: OAuthTokenResponse, previous_refresh: Option<&str>) -> Result<MsaToken> {
    let refresh_token = t
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::Auth("No refresh token returned by Microsoft".into()))?;
    Ok(MsaToken {
        access_token: t.access_token,
        refresh_token,
        expires_at: Utc::now().timestamp() + t.expires_in.unwrap_or(3600),
    })
}

fn xsts_error_message(body: &str) -> String {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("XErr").and_then(|x| x.as_u64()));
    match code {
        Some(2148916233) => "This Microsoft account has no Xbox profile yet. Sign in once on xbox.com to create one.".into(),
        Some(2148916235) => "Xbox Live is not available in your country.".into(),
        Some(2148916236) | Some(2148916237) => "This account needs adult verification (South Korea).".into(),
        Some(2148916238) => "Child accounts must be added to a Microsoft family first.".into(),
        Some(c) => format!("Xbox Live authorization failed (XErr {c})"),
        None => format!("Xbox Live authorization failed: {body}"),
    }
}

/// Turn non-2xx responses into a typed error carrying the body.
pub(crate) async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let url = resp.url().to_string();
    let body = resp.text().await.unwrap_or_default();
    tracing::warn!(%url, status = status.as_u16(), %body, "HTTP request failed");
    Err(Error::Http {
        status: status.as_u16(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xsts_response() {
        let body = r#"{"IssueInstant":"2024-01-01T00:00:00.0000000Z","NotAfter":"2024-01-01T16:00:00.0000000Z","Token":"abc","DisplayClaims":{"xui":[{"gtg":"Player","xid":"2535","uhs":"1234"}]}}"#;
        let r: XblResponse = serde_json::from_str(body).unwrap();
        let t: XblToken = r.into();
        assert_eq!(t.uhs, "1234");
        assert_eq!(t.gamertag.as_deref(), Some("Player"));
        assert_eq!(t.authorization(), "XBL3.0 x=1234;abc");
    }

    #[test]
    fn parses_stream_token() {
        let body = r#"{"offeringSettings":{"allowRegionSelection":false,"regions":[{"name":"WestEurope","baseUri":"https://weu.gssv-play-prodxhome.xboxlive.com","networkTestHostname":null,"isDefault":true,"poolIds":null,"systemUpdateGroups":null,"fallbackPriority":-1}],"selectableServerTypes":null,"clientCloudSettings":{"Environments":[]}},"market":"FR","gsToken":"tok","tokenType":"bearer","durationInSeconds":14400}"#;
        let t: StreamToken = serde_json::from_str(body).unwrap();
        assert_eq!(t.gs_token, "tok");
        assert_eq!(
            t.default_region().unwrap().base_uri,
            "https://weu.gssv-play-prodxhome.xboxlive.com"
        );
    }
}
