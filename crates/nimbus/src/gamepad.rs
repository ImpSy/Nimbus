//! Controllers.
//!
//! A dedicated thread blocks on gilrs (IOKit HID on macOS, evdev on Linux)
//! and forwards every change to the main thread's event loop, which merges
//! it with keyboard emulation and writes the new state to the stream right
//! away (the network thread is woken through mio) — instead of the ~60 Hz
//! polling browsers are limited to.
//!
//! When Nimbus is started from Steam (always the case in Steam Deck game
//! mode), Steam Input exposes every controller as a virtual Xbox pad and
//! lists the physical devices behind them in
//! `SDL_GAMECONTROLLER_IGNORE_DEVICES`; those are skipped so a controller
//! never shows up twice.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use gilrs::{Axis, Button, EventType, GamepadId, Gilrs, GilrsBuilder};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::KeyCode;
use xcloud::protocol::input::{GamepadState, button, stick, trigger};
use xcloud::stream::Command;

use crate::shared::{AppEvent, NavAction, Shared, UiMsg};

const MAX_SLOTS: u8 = 4;
const NAV_REPEAT_DELAY: Duration = Duration::from_millis(400);
const NAV_REPEAT_RATE: Duration = Duration::from_millis(110);
const STICK_NAV_THRESHOLD: f32 = 0.6;

/// A controller change, already translated to the stream's vocabulary.
#[derive(Debug, Clone)]
pub enum PadEvent {
    Connected {
        id: usize,
        name: String,
    },
    Disconnected {
        id: usize,
    },
    Button {
        id: usize,
        bit: u16,
        down: bool,
    },
    Axis {
        id: usize,
        axis: PadAxis,
        value: f32,
    },
}

/// Sticks are -1.0..=1.0 with Y up; triggers 0.0..=1.0.
#[derive(Debug, Clone, Copy)]
pub enum PadAxis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
}

// ---------------------------------------------------------------- reader

/// Start the controller thread. It lives for the whole process.
pub fn spawn_reader(proxy: EventLoopProxy<AppEvent>) {
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || reader(proxy))
        .expect("spawn gamepad thread");
}

/// `0xVVVV/0xPPPP,...` lists, as set by Steam for SDL.
#[derive(Default)]
struct IgnoreList {
    ignore: Vec<(u16, u16)>,
    only: Vec<(u16, u16)>,
}

fn parse_device_list(list: &str) -> Vec<(u16, u16)> {
    let hex = |s: &str| {
        let s = s.trim();
        let s = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s);
        u16::from_str_radix(s, 16).ok()
    };
    list.split(',')
        .filter_map(|e| {
            let (v, p) = e.split_once('/')?;
            Some((hex(v)?, hex(p)?))
        })
        .collect()
}

impl IgnoreList {
    fn from_env() -> Self {
        let var = |name: &str| parse_device_list(&std::env::var(name).unwrap_or_default());
        Self {
            ignore: var("SDL_GAMECONTROLLER_IGNORE_DEVICES"),
            only: var("SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT"),
        }
    }

    fn ignores(&self, vid: u16, pid: u16) -> bool {
        self.ignore.contains(&(vid, pid))
            || (!self.only.is_empty() && !self.only.contains(&(vid, pid)))
    }
}

fn button_bit(b: Button) -> u16 {
    match b {
        Button::South => button::A,
        Button::East => button::B,
        Button::West => button::X,
        Button::North => button::Y,
        Button::Select => button::VIEW,
        Button::Start => button::MENU,
        Button::Mode => button::NEXUS,
        Button::LeftThumb => button::LEFT_THUMB,
        Button::RightThumb => button::RIGHT_THUMB,
        // gilrs: "Trigger" = bumper, "Trigger2" = analog trigger.
        Button::LeftTrigger => button::LEFT_SHOULDER,
        Button::RightTrigger => button::RIGHT_SHOULDER,
        Button::DPadUp => button::DPAD_UP,
        Button::DPadDown => button::DPAD_DOWN,
        Button::DPadLeft => button::DPAD_LEFT,
        Button::DPadRight => button::DPAD_RIGHT,
        _ => 0,
    }
}

fn reader(proxy: EventLoopProxy<AppEvent>) {
    // No gilrs filters: deadzones are applied by us (radial, configurable),
    // and its jitter filter would only add work.
    let mut gilrs = match GilrsBuilder::new().with_default_filters(false).build() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("controllers unavailable: {e}");
            return;
        }
    };
    let ignore = IgnoreList::from_env();
    let mut ignored: HashSet<usize> = HashSet::new();
    let send = |ev: PadEvent| proxy.send_event(AppEvent::Pad(ev)).is_ok();

    // Controllers that were already plugged in (not reported as events on
    // every platform).
    let initial: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
    for id in initial {
        if let Some(ev) = connect(&gilrs, id, &ignore, &mut ignored)
            && !send(ev)
        {
            return;
        }
    }

    loop {
        let Some(ev) = gilrs.next_event_blocking(None) else {
            continue;
        };
        let id: usize = ev.id.into();
        let axis = |axis, value| PadEvent::Axis { id, axis, value };
        let btn = |bit, down| PadEvent::Button { id, bit, down };
        let out: Vec<PadEvent> = match ev.event {
            EventType::Connected => connect(&gilrs, ev.id, &ignore, &mut ignored)
                .into_iter()
                .collect(),
            EventType::Disconnected => {
                if ignored.remove(&id) {
                    continue;
                }
                vec![PadEvent::Disconnected { id }]
            }
            _ if ignored.contains(&id) => continue,
            EventType::ButtonPressed(b, _) | EventType::ButtonReleased(b, _) => {
                let bit = button_bit(b);
                if bit == 0 {
                    continue;
                }
                vec![btn(bit, matches!(ev.event, EventType::ButtonPressed(..)))]
            }
            EventType::ButtonChanged(Button::LeftTrigger2, v, _) => {
                vec![axis(PadAxis::LeftTrigger, v)]
            }
            EventType::ButtonChanged(Button::RightTrigger2, v, _) => {
                vec![axis(PadAxis::RightTrigger, v)]
            }
            EventType::AxisChanged(a, v, _) => match a {
                Axis::LeftStickX => vec![axis(PadAxis::LeftX, v)],
                Axis::LeftStickY => vec![axis(PadAxis::LeftY, v)],
                Axis::RightStickX => vec![axis(PadAxis::RightX, v)],
                Axis::RightStickY => vec![axis(PadAxis::RightY, v)],
                // Hats that the mapping didn't turn into buttons (Y up).
                Axis::DPadX => vec![
                    btn(button::DPAD_LEFT, v < -0.5),
                    btn(button::DPAD_RIGHT, v > 0.5),
                ],
                Axis::DPadY => vec![
                    btn(button::DPAD_UP, v > 0.5),
                    btn(button::DPAD_DOWN, v < -0.5),
                ],
                _ => continue,
            },
            _ => continue,
        };
        for ev in out {
            if !send(ev) {
                return; // event loop gone: shutting down
            }
        }
    }
}

fn connect(
    gilrs: &Gilrs,
    id: GamepadId,
    ignore: &IgnoreList,
    ignored: &mut HashSet<usize>,
) -> Option<PadEvent> {
    let pad = gilrs.gamepad(id);
    let (vid, pid) = (pad.vendor_id().unwrap_or(0), pad.product_id().unwrap_or(0));
    if ignore.ignores(vid, pid) {
        tracing::info!(
            name = pad.name(),
            "ignoring controller {vid:04x}:{pid:04x} (exposed through Steam Input)"
        );
        ignored.insert(id.into());
        return None;
    }
    tracing::debug!(
        name = pad.name(),
        os_name = pad.os_name(),
        mapping = ?pad.mapping_source(),
        "controller {vid:04x}:{pid:04x}"
    );
    Some(PadEvent::Connected {
        id: id.into(),
        name: pad.name().to_owned(),
    })
}

// ---------------------------------------------------------------- state

#[derive(Default, Clone, Copy)]
struct RawState {
    buttons: u16,
    lx: f32,
    ly: f32,
    rx: f32,
    ry: f32,
    lt: f32,
    rt: f32,
}

struct Pad {
    id: usize,
    slot: u8,
    raw: RawState,
    dirty: bool,
    name: String,
}

/// Controller state on the main thread.
pub struct Gamepads {
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

impl Gamepads {
    pub fn new() -> Self {
        Self {
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
    pub fn handle_event(&mut self, ev: PadEvent, shared: &Shared) -> bool {
        match ev {
            PadEvent::Connected { id, name } => {
                if self.pads.iter().any(|p| p.id == id) {
                    return false;
                }
                let Some(slot) = self.free_slot() else {
                    return false;
                };
                tracing::info!(%name, slot, "controller connected");
                self.pads.push(Pad {
                    id,
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
            PadEvent::Disconnected { id } => {
                if let Some(i) = self.pads.iter().position(|p| p.id == id) {
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
            PadEvent::Button { id, bit, down } => {
                if let Some(p) = self.pads.iter_mut().find(|p| p.id == id) {
                    let before = p.raw.buttons;
                    if down {
                        p.raw.buttons |= bit;
                    } else {
                        p.raw.buttons &= !bit;
                    }
                    p.dirty |= p.raw.buttons != before;
                }
            }
            PadEvent::Axis { id, axis, value } => {
                if let Some(p) = self.pads.iter_mut().find(|p| p.id == id) {
                    let v = match axis {
                        PadAxis::LeftX => &mut p.raw.lx,
                        PadAxis::LeftY => &mut p.raw.ly,
                        PadAxis::RightX => &mut p.raw.rx,
                        PadAxis::RightY => &mut p.raw.ry,
                        PadAxis::LeftTrigger => &mut p.raw.lt,
                        PadAxis::RightTrigger => &mut p.raw.rt,
                    };
                    *v = value;
                    p.dirty = true;
                }
            }
        }
        false
    }

    /// Keyboard → virtual controller (slot 0), Greenlight-compatible layout.
    /// Physical keys, so WASD stays WASD on any keyboard layout.
    pub fn handle_key(&mut self, key: KeyCode, down: bool) -> bool {
        use KeyCode as K;
        let amount = |v: f32| if down { v } else { 0.0 };
        let k = &mut self.keyboard;
        let bit = match key {
            K::Enter | K::NumpadEnter => button::A,
            K::Backspace => button::B,
            K::KeyX => button::X,
            K::KeyY => button::Y,
            K::ArrowUp => button::DPAD_UP,
            K::ArrowDown => button::DPAD_DOWN,
            K::ArrowLeft => button::DPAD_LEFT,
            K::ArrowRight => button::DPAD_RIGHT,
            K::BracketLeft => button::LEFT_SHOULDER,
            K::BracketRight => button::RIGHT_SHOULDER,
            K::KeyL => button::LEFT_THUMB,
            K::KeyR => button::RIGHT_THUMB,
            K::KeyM => button::MENU,
            K::KeyV => button::VIEW,
            K::KeyN => button::NEXUS,
            K::Minus => {
                k.lt = amount(1.0);
                0
            }
            K::Equal => {
                k.rt = amount(1.0);
                0
            }
            K::KeyW => {
                k.ly = amount(1.0);
                0
            }
            K::KeyS => {
                k.ly = amount(-1.0);
                0
            }
            K::KeyA => {
                k.lx = amount(-1.0);
                0
            }
            K::KeyD => {
                k.lx = amount(1.0);
                0
            }
            _ => return false,
        };
        if down {
            k.buttons |= bit;
        } else {
            k.buttons &= !bit;
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
        let (lx, ly) = stick(raw.lx, raw.ly, deadzone);
        let (rx, ry) = stick(raw.rx, raw.ry, deadzone);
        GamepadState {
            buttons: raw.buttons,
            left_x: lx,
            left_y: ly,
            right_x: rx,
            right_y: ry,
            left_trigger: trigger(raw.lt),
            right_trigger: trigger(raw.rt),
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
            let pick = |a: f32, b: f32| if b != 0.0 { b } else { a };
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
        let (mut lx, mut ly) = (0f32, 0f32);
        for p in &self.pads {
            buttons |= p.raw.buttons;
            if p.raw.lx.abs() > lx.abs() {
                lx = p.raw.lx;
            }
            if p.raw.ly.abs() > ly.abs() {
                ly = p.raw.ly;
            }
        }
        let action = if buttons & button::DPAD_UP != 0 || ly > STICK_NAV_THRESHOLD {
            Some(NavAction::Up)
        } else if buttons & button::DPAD_DOWN != 0 || ly < -STICK_NAV_THRESHOLD {
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

    /// Next time held navigation / a Nexus tap needs re-evaluating, if any.
    pub fn next_tick(&self) -> Option<Instant> {
        let nav = self.nav_held.map(|(_, since, last)| {
            (since + NAV_REPEAT_DELAY).max(last + NAV_REPEAT_RATE) + Duration::from_millis(1)
        });
        match (nav, self.nexus_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_ignore_list() {
        let l = IgnoreList {
            ignore: parse_device_list("0x28DE/0x1205, 0x054c/0x09cc,bogus,"),
            only: vec![],
        };
        assert_eq!(l.ignore, vec![(0x28de, 0x1205), (0x054c, 0x09cc)]);
        assert!(l.ignores(0x054c, 0x09cc));
        assert!(!l.ignores(0x28de, 0x11ff));
        let l = IgnoreList {
            ignore: vec![],
            only: vec![(0x28de, 0x11ff)],
        };
        assert!(l.ignores(0x054c, 0x09cc));
        assert!(!l.ignores(0x28de, 0x11ff));
    }
}
