//! Controller handling on the main thread.
//!
//! Input is event driven: every SDL gamepad event updates the pad state and
//! the new state is written to the stream immediately (the network thread is
//! woken through mio), instead of the ~60 Hz polling browsers are limited to.

use std::time::{Duration, Instant};

use sdl3::gamepad::{Axis, Button, Gamepad};
use sdl3::joystick::JoystickId;
use sdl3::GamepadSubsystem;
use xcloud::protocol::input::{button, stick_from_sdl, trigger_from_sdl, GamepadState, Vibration};
use xcloud::stream::Command;

use crate::shared::{NavAction, Shared, UiMsg};

const MAX_SLOTS: u8 = 4;
const NAV_REPEAT_DELAY: Duration = Duration::from_millis(400);
const NAV_REPEAT_RATE: Duration = Duration::from_millis(110);
const STICK_NAV_THRESHOLD: i16 = 20_000;

#[derive(Default, Clone, Copy)]
struct RawState {
    buttons: u16,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    lt: i16,
    rt: i16,
}

struct Pad {
    pad: Gamepad,
    id: JoystickId,
    slot: u8,
    raw: RawState,
    dirty: bool,
    name: String,
}

pub struct Gamepads {
    subsystem: GamepadSubsystem,
    pads: Vec<Pad>,
    /// Keyboard-emulated pad merged into slot 0.
    keyboard: RawState,
    keyboard_dirty: bool,
    /// UI navigation state.
    nav_held: Option<(NavAction, Instant, Instant)>,
    menu_combo_armed: bool,
    routed_to_game: bool,
    nexus_until: Option<Instant>,
}

fn button_bit(b: Button) -> u16 {
    match b {
        Button::South => button::A,
        Button::East => button::B,
        Button::West => button::X,
        Button::North => button::Y,
        Button::Back => button::VIEW,
        Button::Start => button::MENU,
        Button::Guide => button::NEXUS,
        Button::LeftStick => button::LEFT_THUMB,
        Button::RightStick => button::RIGHT_THUMB,
        Button::LeftShoulder => button::LEFT_SHOULDER,
        Button::RightShoulder => button::RIGHT_SHOULDER,
        Button::DPadUp => button::DPAD_UP,
        Button::DPadDown => button::DPAD_DOWN,
        Button::DPadLeft => button::DPAD_LEFT,
        Button::DPadRight => button::DPAD_RIGHT,
        // Share / capture button on Series controllers → Nexus is the
        // closest useful mapping; paddles are ignored.
        Button::Misc1 => button::NEXUS,
        _ => 0,
    }
}

impl Gamepads {
    pub fn new(subsystem: GamepadSubsystem) -> Self {
        Self {
            subsystem,
            pads: Vec::new(),
            keyboard: RawState::default(),
            keyboard_dirty: false,
            nav_held: None,
            menu_combo_armed: true,
            routed_to_game: false,
            nexus_until: None,
        }
    }

    pub fn names(&self) -> Vec<(u8, String)> {
        let mut v: Vec<_> = self
            .pads
            .iter()
            .map(|p| (p.slot, format!("P{} · {}", p.slot + 1, p.name)))
            .collect();
        v.sort();
        v
    }

    fn free_slot(&self) -> Option<u8> {
        (0..MAX_SLOTS).find(|s| !self.pads.iter().any(|p| p.slot == *s))
    }

    /// Returns true if the set of pads changed.
    pub fn handle_event(&mut self, ev: &sdl3::event::Event, shared: &Shared) -> bool {
        use sdl3::event::Event;
        match ev {
            Event::GamepadAdded { which, .. } => {
                if self.pads.iter().any(|p| p.id == *which) {
                    return false;
                }
                let Some(slot) = self.free_slot() else {
                    return false;
                };
                match self.subsystem.open(*which) {
                    Ok(pad) => {
                        let name = pad.name().unwrap_or_else(|| "Controller".into());
                        tracing::info!(%name, slot, "controller connected");
                        let _ = pad.set_player_index(slot as u16);
                        self.pads.push(Pad {
                            pad,
                            id: *which,
                            slot,
                            raw: RawState::default(),
                            dirty: true,
                            name,
                        });
                        if let Some(h) = shared.stream.lock().clone() {
                            h.send(Command::GamepadConnected(slot, true));
                        }
                        return true;
                    }
                    Err(e) => tracing::warn!("failed to open controller: {e}"),
                }
            }
            Event::GamepadRemoved { which, .. } => {
                if let Some(i) = self.pads.iter().position(|p| p.id == *which) {
                    let p = self.pads.remove(i);
                    tracing::info!(name = %p.name, "controller disconnected");
                    if let Some(h) = shared.stream.lock().clone() {
                        // Release everything, then unplug (slot 0 stays for keyboard).
                        h.gamepad(p.slot, GamepadState::default());
                        if p.slot != 0 {
                            h.send(Command::GamepadConnected(p.slot, false));
                        }
                    }
                    return true;
                }
            }
            Event::GamepadButtonDown { which, button, .. }
            | Event::GamepadButtonUp { which, button, .. } => {
                let down = matches!(ev, Event::GamepadButtonDown { .. });
                if let Some(p) = self.pads.iter_mut().find(|p| p.id == *which) {
                    let bit = button_bit(*button);
                    if down {
                        p.raw.buttons |= bit;
                    } else {
                        p.raw.buttons &= !bit;
                    }
                    p.dirty = true;
                }
            }
            Event::GamepadAxisMotion {
                which, axis, value, ..
            } => {
                if let Some(p) = self.pads.iter_mut().find(|p| p.id == *which) {
                    match axis {
                        Axis::LeftX => p.raw.lx = *value,
                        Axis::LeftY => p.raw.ly = *value,
                        Axis::RightX => p.raw.rx = *value,
                        Axis::RightY => p.raw.ry = *value,
                        Axis::TriggerLeft => p.raw.lt = *value,
                        Axis::TriggerRight => p.raw.rt = *value,
                    }
                    p.dirty = true;
                }
            }
            _ => {}
        }
        false
    }

    /// Keyboard → virtual controller (slot 0), Greenlight-compatible layout.
    pub fn handle_key(&mut self, key: sdl3::keyboard::Keycode, down: bool) -> bool {
        use sdl3::keyboard::Keycode as K;
        let set_axis = |v: &mut i16, amount: i16| {
            *v = if down { amount } else { 0 };
        };
        let bit = match key {
            K::Return => button::A,
            K::Backspace => button::B,
            K::X => button::X,
            K::Y => button::Y,
            K::Up => button::DPAD_UP,
            K::Down => button::DPAD_DOWN,
            K::Left => button::DPAD_LEFT,
            K::Right => button::DPAD_RIGHT,
            K::LeftBracket => button::LEFT_SHOULDER,
            K::RightBracket => button::RIGHT_SHOULDER,
            K::L => button::LEFT_THUMB,
            K::R => button::RIGHT_THUMB,
            K::M => button::MENU,
            K::V => button::VIEW,
            K::N => button::NEXUS,
            K::Minus => {
                set_axis(&mut self.keyboard.lt, i16::MAX);
                self.keyboard_dirty = true;
                return true;
            }
            K::Equals => {
                set_axis(&mut self.keyboard.rt, i16::MAX);
                self.keyboard_dirty = true;
                return true;
            }
            K::W => {
                set_axis(&mut self.keyboard.ly, i16::MIN);
                self.keyboard_dirty = true;
                return true;
            }
            K::S => {
                set_axis(&mut self.keyboard.ly, i16::MAX);
                self.keyboard_dirty = true;
                return true;
            }
            K::A => {
                set_axis(&mut self.keyboard.lx, i16::MIN);
                self.keyboard_dirty = true;
                return true;
            }
            K::D => {
                set_axis(&mut self.keyboard.lx, i16::MAX);
                self.keyboard_dirty = true;
                return true;
            }
            _ => return false,
        };
        if down {
            self.keyboard.buttons |= bit;
        } else {
            self.keyboard.buttons &= !bit;
        }
        self.keyboard_dirty = true;
        true
    }

    /// Hold the Nexus button on slot 0 for a short tap (Steam Deck has no
    /// usable Xbox button).
    pub fn pulse_nexus(&mut self) {
        self.nexus_until = Some(Instant::now() + Duration::from_millis(150));
        self.keyboard_dirty = true;
    }

    pub fn release_keyboard(&mut self) {
        self.keyboard = RawState::default();
        self.keyboard_dirty = true;
    }

    fn to_state(raw: &RawState, deadzone: f32) -> GamepadState {
        let (lx, ly) = stick_from_sdl(raw.lx, raw.ly, deadzone);
        let (rx, ry) = stick_from_sdl(raw.rx, raw.ry, deadzone);
        GamepadState {
            buttons: raw.buttons,
            left_x: lx,
            left_y: ly,
            right_x: rx,
            right_y: ry,
            left_trigger: trigger_from_sdl(raw.lt),
            right_trigger: trigger_from_sdl(raw.rt),
        }
    }

    fn merged(&self, slot: u8) -> RawState {
        let mut raw = self
            .pads
            .iter()
            .find(|p| p.slot == slot)
            .map(|p| p.raw)
            .unwrap_or_default();
        if slot == 0 {
            let k = self.keyboard;
            raw.buttons |= k.buttons;
            if self.nexus_until.is_some() {
                raw.buttons |= button::NEXUS;
            }
            let pick = |a: i16, b: i16| if b != 0 { b } else { a };
            raw.lx = pick(raw.lx, k.lx);
            raw.ly = pick(raw.ly, k.ly);
            raw.lt = raw.lt.max(k.lt);
            raw.rt = raw.rt.max(k.rt);
        }
        raw
    }

    /// Called after each batch of events: forwards changed states to the
    /// stream, or turns them into UI navigation.
    pub fn flush(&mut self, shared: &Shared, ui: &crossbeam_channel::Sender<UiMsg>) {
        // Client menu combo: View + Menu together, on any pad.
        let combo = self
            .pads
            .iter()
            .map(|p| p.raw.buttons)
            .chain([self.keyboard.buttons])
            .any(|b| b & button::VIEW != 0 && b & button::MENU != 0);
        if combo && self.menu_combo_armed {
            self.menu_combo_armed = false;
            let _ = ui.send(UiMsg::ToggleMenu);
            // Make sure the game doesn't see the combo held.
            if let Some(h) = shared.stream.lock().clone() {
                for s in 0..MAX_SLOTS {
                    if self.pads.iter().any(|p| p.slot == s) || s == 0 {
                        h.gamepad(s, GamepadState::default());
                    }
                }
            }
            for p in &mut self.pads {
                p.dirty = false;
            }
            self.keyboard_dirty = false;
            return;
        }
        if !combo {
            self.menu_combo_armed = true;
        }

        if let Some(t) = self.nexus_until
            && Instant::now() >= t
        {
            self.nexus_until = None;
            self.keyboard_dirty = true;
        }
        if let Some(handle) = shared.game_input() {
            if !self.routed_to_game {
                // Coming back from the menu / starting a stream: resync
                // whatever is currently held.
                self.routed_to_game = true;
                for p in &mut self.pads {
                    p.dirty = true;
                }
                self.keyboard_dirty = true;
            }
            let deadzone = shared.deadzone();
            let mut slots: Vec<u8> = self
                .pads
                .iter_mut()
                .filter_map(|p| std::mem::take(&mut p.dirty).then_some(p.slot))
                .collect();
            if std::mem::take(&mut self.keyboard_dirty) && !slots.contains(&0) {
                slots.push(0);
            }
            for slot in slots {
                handle.gamepad(slot, Self::to_state(&self.merged(slot), deadzone));
            }
            self.nav_held = None;
        } else {
            self.routed_to_game = false;
            for p in &mut self.pads {
                p.dirty = false;
            }
            self.keyboard_dirty = false;
            self.ui_navigation(ui);
        }
    }

    fn ui_navigation(&mut self, ui: &crossbeam_channel::Sender<UiMsg>) {
        // Combine all pads for navigation.
        let mut buttons = 0u16;
        let (mut lx, mut ly) = (0i16, 0i16);
        for p in &self.pads {
            buttons |= p.raw.buttons;
            if p.raw.lx.unsigned_abs() > lx.unsigned_abs() {
                lx = p.raw.lx;
            }
            if p.raw.ly.unsigned_abs() > ly.unsigned_abs() {
                ly = p.raw.ly;
            }
        }
        let action = if buttons & button::DPAD_UP != 0 || ly < -STICK_NAV_THRESHOLD {
            Some(NavAction::Up)
        } else if buttons & button::DPAD_DOWN != 0 || ly > STICK_NAV_THRESHOLD {
            Some(NavAction::Down)
        } else if buttons & button::DPAD_LEFT != 0 || lx < -STICK_NAV_THRESHOLD {
            Some(NavAction::Left)
        } else if buttons & button::DPAD_RIGHT != 0 || lx > STICK_NAV_THRESHOLD {
            Some(NavAction::Right)
        } else if buttons & button::A != 0 {
            Some(NavAction::Activate)
        } else if buttons & button::B != 0 {
            Some(NavAction::Back)
        } else if buttons & button::LEFT_SHOULDER != 0 {
            Some(NavAction::PrevTab)
        } else if buttons & button::RIGHT_SHOULDER != 0 {
            Some(NavAction::NextTab)
        } else if buttons & button::Y != 0 {
            Some(NavAction::Secondary)
        } else {
            None
        };
        let now = Instant::now();
        match (action, self.nav_held) {
            (None, _) => self.nav_held = None,
            (Some(a), Some((held, since, last))) if held == a => {
                let repeatable = matches!(
                    a,
                    NavAction::Up | NavAction::Down | NavAction::Left | NavAction::Right
                );
                if repeatable
                    && now.duration_since(since) > NAV_REPEAT_DELAY
                    && now.duration_since(last) > NAV_REPEAT_RATE
                {
                    let _ = ui.send(UiMsg::Nav(a));
                    self.nav_held = Some((a, since, now));
                }
            }
            (Some(a), _) => {
                let _ = ui.send(UiMsg::Nav(a));
                self.nav_held = Some((a, now, now));
            }
        }
    }

    /// Whether held navigation needs periodic re-evaluation (key repeat).
    pub fn needs_tick(&self) -> bool {
        self.nav_held.is_some() || self.nexus_until.is_some()
    }

    pub fn rumble(&mut self, v: &Vibration) {
        let Some(p) = self.pads.iter_mut().find(|p| p.slot == v.gamepad_index) else {
            return;
        };
        let scale = |pct: u8| ((pct.min(100) as u32 * 65535) / 100) as u16;
        let duration = if v.duration_ms == 0 {
            0
        } else {
            v.duration_ms as u32 + 20
        };
        let _ = p
            .pad
            .set_rumble(scale(v.left_motor), scale(v.right_motor), duration);
        let _ = p
            .pad
            .set_rumble_triggers(scale(v.left_trigger), scale(v.right_trigger), duration);
    }

    pub fn stop_rumble(&mut self) {
        for p in &mut self.pads {
            let _ = p.pad.set_rumble(0, 0, 0);
            let _ = p.pad.set_rumble_triggers(0, 0, 0);
        }
    }
}
