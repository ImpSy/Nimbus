//! Media decoding for the streaming client.

pub mod audio;
pub mod video;

use std::sync::Once;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("decoder init: {0}")]
    Init(String),
    #[error("decode: {0}")]
    Decode(String),
}

pub(crate) fn init_ffmpeg() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        ffmpeg_sys_next::av_log_set_level(ffmpeg_sys_next::AV_LOG_ERROR as i32);
    });
}
