//! Media decoding for the streaming client.
//!
//! Supported platforms: Apple Silicon Macs (VideoToolbox) and the Steam Deck
//! (x86_64 Linux, VA-API).

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
)))]
compile_error!(
    "Nimbus supports Apple Silicon Macs (aarch64-apple-darwin) and the Steam Deck (x86_64-unknown-linux-gnu) only."
);

pub mod audio;
pub mod video;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("decoder init: {0}")]
    Init(String),
    #[error("decode: {0}")]
    Decode(String),
}

#[cfg(target_os = "linux")]
pub(crate) fn init_ffmpeg() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        ffmpeg_sys_next::av_log_set_level(ffmpeg_sys_next::AV_LOG_ERROR as i32);
    });
}
