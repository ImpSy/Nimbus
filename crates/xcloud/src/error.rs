use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("your session expired, please sign in again ({0})")]
    ReauthRequired(String),
    #[error("not signed in")]
    NotSignedIn,
    #[error("{0}")]
    Session(String),
    #[error("WebRTC: {0}")]
    Rtc(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("cancelled")]
    Cancelled,
}

impl From<str0m::RtcError> for Error {
    fn from(e: str0m::RtcError) -> Self {
        Error::Rtc(e.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
