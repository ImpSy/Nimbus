//! `X-MS-Device-Info` header generation.
//!
//! The streaming backend picks the stream resolution / bitrate tier from the
//! reported client OS. Known mapping (also used by Better xCloud):
//!
//! | osName    | result                         |
//! |-----------|--------------------------------|
//! | `android` | 720p                           |
//! | `windows` | 1080p                          |
//! | `tizen`   | 1080p with a higher bitrate    |

use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Resolution {
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "1080p")]
    P1080,
    /// 1080p, high bitrate profile.
    #[serde(rename = "1080p-hq")]
    #[default]
    P1080Hq,
}

impl Resolution {
    pub const ALL: [Resolution; 3] = [Resolution::P720, Resolution::P1080, Resolution::P1080Hq];

    pub fn os_name(self) -> &'static str {
        match self {
            Resolution::P720 => "android",
            Resolution::P1080 => "windows",
            Resolution::P1080Hq => "tizen",
        }
    }

    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Resolution::P720 => (1280, 720),
            Resolution::P1080 | Resolution::P1080Hq => (1920, 1080),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P720 => "720p",
            Resolution::P1080 => "1080p",
            Resolution::P1080Hq => "1080p (high bitrate)",
        }
    }
}

pub fn device_info(resolution: Resolution) -> String {
    json!({
        "appInfo": {
            "env": {
                "clientAppId": "www.xbox.com",
                "clientAppType": "browser",
                "clientAppVersion": "26.1.97",
                "clientSdkVersion": "10.3.7",
                "httpEnvironment": "prod",
                "sdkInstallId": "",
            }
        },
        "dev": {
            "os": { "name": resolution.os_name(), "ver": "22631.2715", "platform": "desktop" },
            "hw": { "make": "Microsoft", "model": "unknown", "sdktype": "web" },
            "browser": { "browserName": "chrome", "browserVersion": "140.0.3485.54" },
            "displayInfo": {
                "dimensions": { "widthInPixels": 4096, "heightInPixels": 2160 },
                "pixelDensity": { "dpiX": 1, "dpiY": 1 },
            },
        },
    })
    .to_string()
}
