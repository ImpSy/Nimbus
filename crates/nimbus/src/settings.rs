//! Persisted user settings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use xcloud::device_info::Resolution;
use xcloud::stream::CodecPreference;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DecoderPref {
    /// Hardware decoding (VideoToolbox / VA-API) with software fallback.
    #[default]
    Hardware,
    /// CPU decoding. On fast CPUs (Apple Silicon) this can shave ~2 ms.
    Software,
}

impl DecoderPref {
    pub const ALL: [DecoderPref; 2] = [DecoderPref::Hardware, DecoderPref::Software];
    pub fn label(self) -> &'static str {
        match self {
            DecoderPref::Hardware => "Hardware (VideoToolbox / VA-API)",
            DecoderPref::Software => "Software (CPU)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PresentPref {
    /// Mailbox when available (no tearing, newest frame wins), else V-Sync
    /// with a single frame in flight.
    #[default]
    LowLatency,
    /// Classic V-Sync (FIFO). Smoothest, ~1 frame more latency.
    VSync,
    /// No sync at all. Lowest latency, may tear.
    Immediate,
}

impl PresentPref {
    pub const ALL: [PresentPref; 3] = [Self::LowLatency, Self::VSync, Self::Immediate];
    pub fn label(self) -> &'static str {
        match self {
            Self::LowLatency => "Low latency (mailbox)",
            Self::VSync => "V-Sync",
            Self::Immediate => "Immediate (may tear)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub resolution: Resolution,
    pub codec: CodecPreference,
    /// 0 = unlimited.
    pub max_bitrate_mbps: u32,
    pub decoder: DecoderPref,
    pub present: PresentPref,
    /// 0.0 ..= 1.0 contrast-adaptive sharpening strength.
    pub sharpness: f32,
    pub show_stats: bool,
    pub fullscreen_on_stream: bool,
    pub start_fullscreen: bool,
    /// Radial stick deadzone, 0.0 ..= 0.3.
    pub deadzone: f32,
    pub vibration: bool,
    /// Target audio buffer in milliseconds.
    pub audio_buffer_ms: u32,
    pub locale: String,
    /// Preferred xCloud region name (None = server default).
    pub xcloud_region: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution: Resolution::P1080Hq,
            codec: CodecPreference::Main,
            max_bitrate_mbps: 0,
            decoder: DecoderPref::Hardware,
            present: PresentPref::LowLatency,
            sharpness: 0.35,
            show_stats: false,
            fullscreen_on_stream: true,
            start_fullscreen: false,
            deadzone: 0.08,
            vibration: true,
            audio_buffer_ms: 40,
            locale: "en-US".into(),
            xcloud_region: None,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(data) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        if let Ok(dir) = std::env::var("NIMBUS_HOME") {
            let base = PathBuf::from(dir);
            return Self {
                config_dir: base.join("config"),
                cache_dir: base.join("cache"),
            };
        }
        let dirs = directories::ProjectDirs::from("dev", "nimbus", "Nimbus");
        match dirs {
            Some(d) => Self {
                config_dir: d.config_dir().to_owned(),
                cache_dir: d.cache_dir().to_owned(),
            },
            None => Self {
                config_dir: PathBuf::from(".nimbus/config"),
                cache_dir: PathBuf::from(".nimbus/cache"),
            },
        }
    }

    pub fn settings(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn account(&self) -> PathBuf {
        self.config_dir.join("account.json")
    }
}
