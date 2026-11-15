//! Binary protocol of the `input` data channel.
//!
//! Packet layout (little endian):
//!
//! ```text
//! u16 report_type   bitmask of the sections present
//! u32 sequence
//! f64 timestamp_ms
//! [metadata section]   if report_type & METADATA
//! [gamepad section]    if report_type & GAMEPAD
//! [keyboard section]   if report_type & KEYBOARD
//! [u8 max_touchpoints] if report_type == CLIENT_METADATA
//! ```

pub mod report {
    pub const METADATA: u16 = 1;
    pub const GAMEPAD: u16 = 2;
    pub const POINTER: u16 = 4;
    pub const CLIENT_METADATA: u16 = 8;
    pub const SERVER_METADATA: u16 = 16;
    pub const MOUSE: u16 = 32;
    pub const KEYBOARD: u16 = 64;
    pub const VIBRATION: u16 = 128;
}

/// Gamepad button bits (XInput-like, as used by the stream protocol).
pub mod button {
    pub const NEXUS: u16 = 1 << 1;
    pub const MENU: u16 = 1 << 2;
    pub const VIEW: u16 = 1 << 3;
    pub const A: u16 = 1 << 4;
    pub const B: u16 = 1 << 5;
    pub const X: u16 = 1 << 6;
    pub const Y: u16 = 1 << 7;
    pub const DPAD_UP: u16 = 1 << 8;
    pub const DPAD_DOWN: u16 = 1 << 9;
    pub const DPAD_LEFT: u16 = 1 << 10;
    pub const DPAD_RIGHT: u16 = 1 << 11;
    pub const LEFT_SHOULDER: u16 = 1 << 12;
    pub const RIGHT_SHOULDER: u16 = 1 << 13;
    pub const LEFT_THUMB: u16 = 1 << 14;
    pub const RIGHT_THUMB: u16 = 1 << 15;
}

/// Full state of one gamepad. Stick Y axes are "up is positive" (XInput).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
    pub buttons: u16,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u16,
    pub right_trigger: u16,
}

impl GamepadState {
    pub fn pressed(&self, b: u16) -> bool {
        self.buttons & b != 0
    }
}

/// Per-video-frame timing report sent back to the server (used for its
/// latency / QoS estimation). Times are milliseconds on our stream clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameMetadata {
    /// RTP timestamp of the frame.
    pub server_data_key: u32,
    pub first_packet_arrival_ms: u32,
    pub submitted_ms: u32,
    pub decoded_ms: u32,
    pub rendered_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Windows virtual-key code.
    pub vkey: u8,
    pub pressed: bool,
}

/// Rumble command from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vibration {
    pub gamepad_index: u8,
    /// 0..=100
    pub left_motor: u8,
    pub right_motor: u8,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub duration_ms: u16,
    pub delay_ms: u16,
    pub repeat: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerInput {
    Vibration(Vibration),
    VideoSize { width: u32, height: u32 },
}

/// Parse a packet received on the input channel. `version` is the
/// negotiated input protocol version (v8+ uses a 2-byte report type, older
/// versions a single byte).
pub fn parse_server_packet(data: &[u8], version: u8) -> Option<ServerInput> {
    let kind = *data.first()? as u16;
    let o = if version >= 8 { 2 } else { 1 };
    if kind & report::VIBRATION != 0 {
        let d = data.get(o..o + 11)?;
        // d[0] = rumble type (0 = four motor rumble)
        return Some(ServerInput::Vibration(Vibration {
            gamepad_index: d[1],
            left_motor: d[2],
            right_motor: d[3],
            left_trigger: d[4],
            right_trigger: d[5],
            duration_ms: u16::from_le_bytes([d[6], d[7]]),
            delay_ms: u16::from_le_bytes([d[8], d[9]]),
            repeat: d[10],
        }));
    }
    if kind & report::SERVER_METADATA != 0 {
        let d = data.get(o..o + 8)?;
        let height = u32::from_le_bytes(d[0..4].try_into().ok()?);
        let width = u32::from_le_bytes(d[4..8].try_into().ok()?);
        return Some(ServerInput::VideoSize { width, height });
    }
    None
}

/// Builds input packets with a monotonically increasing sequence number.
#[derive(Debug)]
pub struct InputEncoder {
    sequence: u32,
    version: u8,
}

impl Default for InputEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEncoder {
    /// Encoder for the latest protocol (v8+, 2-byte report type).
    pub fn new() -> Self {
        Self::with_version(9)
    }

    pub fn with_version(version: u8) -> Self {
        Self {
            sequence: 0,
            version,
        }
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    fn header(&self, buf: &mut Vec<u8>, report_type: u16, sequence: u32, timestamp_ms: f64) {
        if self.version >= 8 {
            buf.extend_from_slice(&report_type.to_le_bytes());
        } else {
            buf.push(report_type as u8);
        }
        buf.extend_from_slice(&sequence.to_le_bytes());
        buf.extend_from_slice(&timestamp_ms.to_le_bytes());
    }

    /// First packet sent once the channel is up.
    pub fn client_metadata(&mut self, max_touchpoints: u8, timestamp_ms: f64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(15);
        self.header(&mut buf, report::CLIENT_METADATA, 0, timestamp_ms);
        buf.push(max_touchpoints);
        buf
    }

    /// Encode a report. Returns `None` if there is nothing to send.
    pub fn encode(
        &mut self,
        timestamp_ms: f64,
        metadata: &[FrameMetadata],
        gamepads: &[(u8, GamepadState)],
        keys: &[KeyEvent],
    ) -> Option<Vec<u8>> {
        let mut report_type = 0u16;
        if !metadata.is_empty() {
            report_type |= report::METADATA;
        }
        if !gamepads.is_empty() {
            report_type |= report::GAMEPAD;
        }
        if !keys.is_empty() {
            report_type |= report::KEYBOARD;
        }
        if report_type == 0 {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);

        let mut buf = Vec::with_capacity(
            14 + 1 + metadata.len() * 28 + 1 + gamepads.len() * 23 + 1 + keys.len() * 3,
        );
        self.header(&mut buf, report_type, self.sequence, timestamp_ms);

        if !metadata.is_empty() {
            let metadata = &metadata[..metadata.len().min(255)];
            buf.push(metadata.len() as u8);
            let now = timestamp_ms as u32;
            for m in metadata {
                for v in [
                    m.server_data_key,
                    m.first_packet_arrival_ms,
                    m.submitted_ms,
                    m.decoded_ms,
                    m.rendered_ms,
                    now,
                    now,
                ] {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
        }

        if !gamepads.is_empty() {
            buf.push(gamepads.len() as u8);
            for (index, g) in gamepads {
                buf.push(*index);
                buf.extend_from_slice(&g.buttons.to_le_bytes());
                buf.extend_from_slice(&g.left_x.to_le_bytes());
                buf.extend_from_slice(&g.left_y.to_le_bytes());
                buf.extend_from_slice(&g.right_x.to_le_bytes());
                buf.extend_from_slice(&g.right_y.to_le_bytes());
                buf.extend_from_slice(&g.left_trigger.to_le_bytes());
                buf.extend_from_slice(&g.right_trigger.to_le_bytes());
                // PhysicalPhysicality (LE) and VirtualPhysicality (BE), both 1,
                // byte-for-byte what the web client sends.
                buf.extend_from_slice(&1u32.to_le_bytes());
                buf.extend_from_slice(&1u32.to_be_bytes());
            }
        }

        if !keys.is_empty() {
            buf.push(keys.len() as u8);
            for k in keys {
                buf.push(2); // VKey
                buf.push(k.pressed as u8);
                buf.push(k.vkey);
            }
        }

        Some(buf)
    }
}

/// Convert a stick position (-1.0..=1.0 per axis, Y up positive) into the
/// stream's XInput range, applying a radial deadzone.
pub fn stick(x: f32, y: f32, deadzone: f32) -> (i16, i16) {
    let (fx, fy) = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));
    let mag = (fx * fx + fy * fy).sqrt();
    if mag <= deadzone || mag == 0.0 {
        return (0, 0);
    }
    // Rescale so the output starts at 0 right outside the deadzone.
    let scaled = ((mag - deadzone) / (1.0 - deadzone)).min(1.0);
    let k = scaled / mag;
    let to_i16 = |v: f32| (v * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
    (to_i16(fx * k), to_i16(fy * k))
}

/// Trigger travel (0.0..=1.0) to the stream's 0..=65535 range.
pub fn trigger(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_metadata_packet() {
        let mut e = InputEncoder::new();
        let p = e.client_metadata(1, 0.0);
        assert_eq!(p.len(), 15);
        assert_eq!(&p[0..2], &[8, 0]);
        assert_eq!(p[14], 1);
    }

    #[test]
    fn gamepad_packet_layout() {
        let mut e = InputEncoder::new();
        let g = GamepadState {
            buttons: button::A | button::NEXUS,
            left_x: 100,
            left_y: -100,
            right_x: 0,
            right_y: 0,
            left_trigger: 65535,
            right_trigger: 0,
        };
        let p = e.encode(12.5, &[], &[(0, g)], &[]).unwrap();
        // 14 header + 1 count + 23 per pad
        assert_eq!(p.len(), 38);
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), report::GAMEPAD);
        assert_eq!(u32::from_le_bytes(p[2..6].try_into().unwrap()), 1);
        assert_eq!(f64::from_le_bytes(p[6..14].try_into().unwrap()), 12.5);
        assert_eq!(p[14], 1); // count
        assert_eq!(p[15], 0); // index
        assert_eq!(u16::from_le_bytes([p[16], p[17]]), 0x12);
        assert_eq!(i16::from_le_bytes([p[18], p[19]]), 100);
        assert_eq!(i16::from_le_bytes([p[20], p[21]]), -100);
        assert_eq!(u16::from_le_bytes([p[26], p[27]]), 65535);
        assert_eq!(&p[30..34], &[1, 0, 0, 0]);
        assert_eq!(&p[34..38], &[0, 0, 0, 1]);
        // sequence increments
        let p2 = e.encode(13.0, &[], &[(0, g)], &[]).unwrap();
        assert_eq!(u32::from_le_bytes(p2[2..6].try_into().unwrap()), 2);
    }

    #[test]
    fn metadata_and_gamepad_combined() {
        let mut e = InputEncoder::new();
        let m = FrameMetadata {
            server_data_key: 42,
            ..Default::default()
        };
        let p = e
            .encode(1.0, &[m, m], &[(0, GamepadState::default())], &[])
            .unwrap();
        assert_eq!(p.len(), 14 + 1 + 2 * 28 + 1 + 23);
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 3);
        assert_eq!(p[14], 2);
        assert_eq!(u32::from_le_bytes(p[15..19].try_into().unwrap()), 42);
    }

    #[test]
    fn legacy_v7_header() {
        let mut e = InputEncoder::with_version(7);
        let p = e
            .encode(1.0, &[], &[(0, GamepadState::default())], &[])
            .unwrap();
        assert_eq!(p.len(), 13 + 1 + 23);
        assert_eq!(p[0], report::GAMEPAD as u8);
        assert_eq!(u32::from_le_bytes(p[1..5].try_into().unwrap()), 1);
        // v7 vibration: 1-byte type + 11 bytes
        let v = [128, 0, 1, 10, 20, 30, 40, 100, 0, 0, 0, 1];
        assert!(matches!(
            parse_server_packet(&v, 7),
            Some(ServerInput::Vibration(Vibration {
                gamepad_index: 1,
                left_motor: 10,
                duration_ms: 100,
                repeat: 1,
                ..
            }))
        ));
    }

    #[test]
    fn parses_vibration() {
        let data = [128, 0, 0, 0, 50, 100, 0, 25, 200, 0, 0, 0, 0];
        match parse_server_packet(&data, 9) {
            Some(ServerInput::Vibration(v)) => {
                assert_eq!(v.left_motor, 50);
                assert_eq!(v.right_motor, 100);
                assert_eq!(v.right_trigger, 25);
                assert_eq!(v.duration_ms, 200);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deadzone() {
        assert_eq!(stick(0.03, 0.03, 0.1), (0, 0));
        assert_eq!(stick(1.0, 0.0, 0.1), (32767, 0));
        assert_eq!(stick(0.0, 1.0, 0.1), (0, 32767)); // up is positive
        assert_eq!(stick(0.0, -1.0, 0.1), (0, -32767));
        assert_eq!(trigger(0.0), 0);
        assert_eq!(trigger(1.0), 65535);
    }
}
