//! Native client for Xbox Cloud Gaming (xCloud) and Xbox Remote Play (xHome).
//!
//! * [`auth`] / [`account`]: Microsoft + Xbox Live sign-in and token caching.
//! * [`api`]: GSSV REST API (consoles, titles, session lifecycle).
//! * [`catalog`]: Game Pass catalog metadata / artwork.
//! * [`stream`]: WebRTC transport (str0m) and the stream data-channel protocols.

pub mod account;
pub mod api;
pub mod auth;
pub mod catalog;
pub mod device_info;
pub mod error;
pub mod ice;
pub mod protocol;
pub mod stream;

pub use error::{Error, Result};
