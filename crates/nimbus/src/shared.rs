//! Messages and state shared between the main (SDL) thread and the UI /
//! render thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use xcloud::protocol::input::Vibration;
use xcloud::stream::StreamHandle;

/// Main thread → UI thread.
#[derive(Debug, Clone)]
pub enum UiMsg {
    Input(UiInput),
    Resize { width: u32, height: u32, scale: f32 },
    Nav(NavAction),
    ToggleMenu,
    ToggleStats,
    Gamepads(Vec<(u8, String)>),
    Quit,
}

#[derive(Debug, Clone)]
pub enum UiInput {
    PointerMoved(egui::Pos2),
    PointerButton {
        pos: egui::Pos2,
        button: egui::PointerButton,
        pressed: bool,
    },
    PointerGone,
    Scroll(egui::Vec2),
    Key {
        key: egui::Key,
        pressed: bool,
        modifiers: egui::Modifiers,
    },
    Text(String),
    Focus(bool),
}

/// Controller-driven UI navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    Up,
    Down,
    Left,
    Right,
    Activate,
    Back,
    PrevTab,
    NextTab,
    Secondary,
}

/// UI thread → main thread.
#[derive(Debug, Clone)]
pub enum MainMsg {
    Rumble(Vibration),
    StopRumble,
    SetFullscreen(bool),
    ToggleFullscreen,
    TextInput(bool),
    ShowCursor(bool),
    /// Tap the Xbox (Nexus) button on controller 1.
    PressNexus,
    Quit,
}

pub struct Shared {
    /// Set while a stream is live; gamepad input goes straight to it.
    pub stream: Mutex<Option<StreamHandle>>,
    /// Overlay menu open: controller drives the UI instead of the game.
    pub menu_open: AtomicBool,
    deadzone_bits: AtomicU32,
    pub vibration: AtomicBool,
}

impl Shared {
    pub fn new(deadzone: f32, vibration: bool) -> Self {
        Self {
            stream: Mutex::new(None),
            menu_open: AtomicBool::new(false),
            deadzone_bits: AtomicU32::new(deadzone.to_bits()),
            vibration: AtomicBool::new(vibration),
        }
    }

    pub fn deadzone(&self) -> f32 {
        f32::from_bits(self.deadzone_bits.load(Ordering::Relaxed))
    }

    pub fn set_deadzone(&self, v: f32) {
        self.deadzone_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Stream handle if input should currently go to the game.
    pub fn game_input(&self) -> Option<StreamHandle> {
        if self.menu_open.load(Ordering::Relaxed) {
            return None;
        }
        self.stream.lock().clone()
    }

    pub fn is_streaming(&self) -> bool {
        self.stream.lock().is_some()
    }
}
