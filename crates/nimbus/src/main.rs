//! Nimbus — native, low-latency Xbox Remote Play & Cloud Gaming client.
//!
//! Thread layout:
//! * **main**: winit event loop (window, keyboard, mouse). Controller and
//!   keyboard state goes straight to the network thread.
//! * **gamepads**: blocks on gilrs and wakes the main loop on every change.
//! * **ui**: egui + wgpu rendering and the app state machine.
//! * **xcloud-net**: WebRTC (str0m) + data-channel protocols.
//! * **video-decode** / **audio**: media decoding.
//! * tokio workers: HTTP APIs.

mod audio_out;
mod gamepad;
mod pipeline;
mod render;
mod settings;
mod shared;
mod ui;

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use raw_window_handle::HasDisplayHandle;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::gamepad::Gamepads;
use crate::render::SendDisplayHandle;
use crate::settings::{Paths, Settings};
use crate::shared::{AppEvent, MainMsg, MainTx, Shared, UiInput, UiMsg};

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,str0m=warn,is=warn")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn egui_key(key: &Key) -> Option<egui::Key> {
    match key {
        // winit's named keys use the same names as egui ("ArrowUp", "Enter"…).
        Key::Named(n) => egui::Key::from_name(&format!("{n:?}")),
        Key::Character(c) => egui::Key::from_name(c),
        _ => None,
    }
}

fn egui_modifiers(m: ModifiersState) -> egui::Modifiers {
    let mac = cfg!(target_os = "macos");
    egui::Modifiers {
        alt: m.alt_key(),
        ctrl: m.control_key(),
        shift: m.shift_key(),
        mac_cmd: mac && m.super_key(),
        command: if mac { m.super_key() } else { m.control_key() },
    }
}

/// Printable text only (macOS reports function keys as private-use chars).
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(c))
        .collect()
}

/// Debug automation: NIMBUS_TEST_INPUT=a|y toggles that key every second
/// while streaming.
struct TestInput {
    key: KeyCode,
    at: Instant,
    down: bool,
}

/// The main thread: owns the window and turns OS input into UI messages
/// and controller state.
struct Shell {
    settings: Settings,
    paths: Paths,
    proxy: EventLoopProxy<AppEvent>,
    shared: Arc<Shared>,
    ui_tx: Sender<UiMsg>,
    ui_rx: Option<Receiver<UiMsg>>,
    window: Option<Arc<Window>>,
    ui_thread: Option<JoinHandle<()>>,
    pads: Gamepads,
    modifiers: ModifiersState,
    /// Last pointer position, in egui points.
    pointer: egui::Pos2,
    test_input: Option<TestInput>,
    error: Option<anyhow::Error>,
}

impl Shell {
    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let size = LogicalSize::new(1280.0, 800.0);
        let mut attrs = Window::default_attributes()
            .with_title("Nimbus")
            .with_inner_size(size)
            .with_min_inner_size(LogicalSize::new(720.0, 450.0))
            .with_fullscreen(
                self.settings
                    .start_fullscreen
                    .then_some(Fullscreen::Borderless(None)),
            );
        if let Some(m) = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
        {
            let s = size.to_physical::<i32>(m.scale_factor());
            let (mw, mh) = (m.size().width as i32, m.size().height as i32);
            attrs = attrs.with_position(PhysicalPosition::new(
                m.position().x + (mw - s.width).max(0) / 2,
                m.position().y + (mh - s.height).max(0) / 2,
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = attrs.with_name("nimbus", "Nimbus");
        }
        let window = Arc::new(event_loop.create_window(attrs).context("create window")?);

        // wgpu instance + surface are created on the main thread.
        let raw_display = window.display_handle()?.as_raw();
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
                Box::new(SendDisplayHandle(raw_display)),
            ));
        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;

        let px = window.inner_size();
        let init = ui::UiInit {
            instance,
            surface,
            width: px.width,
            height: px.height,
            scale: window.scale_factor() as f32,
            ui_rx: self.ui_rx.take().context("UI already started")?,
            main_tx: MainTx(self.proxy.clone()),
            shared: self.shared.clone(),
            settings: self.settings.clone(),
            paths: self.paths.clone(),
        };
        let proxy = self.proxy.clone();
        self.ui_thread = Some(
            std::thread::Builder::new()
                .name("ui".into())
                .spawn(move || {
                    // Whatever happens to the UI thread, the app quits with it.
                    struct QuitOnExit(EventLoopProxy<AppEvent>);
                    impl Drop for QuitOnExit {
                        fn drop(&mut self) {
                            let _ = self.0.send_event(AppEvent::Main(MainMsg::Quit));
                        }
                    }
                    let _quit = QuitOnExit(proxy);
                    if let Err(e) = ui::run(init) {
                        tracing::error!("UI thread failed: {e:#}");
                    }
                })?,
        );
        gamepad::spawn_reader(self.proxy.clone());
        self.window = Some(window);
        Ok(())
    }

    fn ui(&self, msg: UiMsg) {
        let _ = self.ui_tx.send(msg);
    }

    fn send_resize(&self) {
        if let Some(w) = &self.window {
            let px = w.inner_size();
            self.ui(UiMsg::Resize {
                width: px.width,
                height: px.height,
                scale: w.scale_factor() as f32,
            });
        }
    }

    fn scale(&self) -> f32 {
        self.window
            .as_ref()
            .map_or(1.0, |w| w.scale_factor() as f32)
            .max(0.5)
    }

    fn set_fullscreen(&self, on: bool) {
        if let Some(w) = &self.window {
            w.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_some())
    }

    /// Steam Deck: show / hide Steam's on-screen keyboard while a text field
    /// has focus (the same deep link SDL uses).
    fn text_input(&self, on: bool) {
        if cfg!(target_os = "linux") && std::env::var_os("SteamDeck").is_some_and(|v| v == "1") {
            let url = if on {
                "steam://open/keyboard?XPosition=0&YPosition=0&Width=0&Height=0&Mode=0"
            } else {
                "steam://close/keyboard"
            };
            if let Ok(mut child) = std::process::Command::new("xdg-open").arg(url).spawn() {
                std::thread::spawn(move || child.wait());
            }
        }
    }

    fn on_key(&mut self, ev: KeyEvent) {
        let down = ev.state == ElementState::Pressed;
        let code = match ev.physical_key {
            PhysicalKey::Code(c) => Some(c),
            PhysicalKey::Unidentified(_) => None,
        };
        // Global hotkeys.
        if down && !ev.repeat {
            match code {
                Some(KeyCode::F11) => {
                    self.set_fullscreen(!self.is_fullscreen());
                    return;
                }
                Some(KeyCode::F1) => {
                    self.ui(UiMsg::ToggleStats);
                    return;
                }
                Some(KeyCode::Escape | KeyCode::F10) if self.shared.is_streaming() => {
                    self.pads.release_keyboard();
                    self.pads.flush(&self.shared, &self.ui_tx);
                    self.ui(UiMsg::ToggleMenu);
                    return;
                }
                _ => {}
            }
        }
        if self.shared.game_input().is_some() {
            if !ev.repeat
                && let Some(c) = code
            {
                self.pads.handle_key(c, down);
            }
            return;
        }
        if let Some(key) = egui_key(&ev.logical_key) {
            self.ui(UiMsg::Input(UiInput::Key {
                key,
                pressed: down,
                modifiers: egui_modifiers(self.modifiers),
            }));
        }
        if down
            && !self.modifiers.control_key()
            && !self.modifiers.super_key()
            && let Some(text) = ev.text.as_deref().map(printable)
            && !text.is_empty()
        {
            self.ui(UiMsg::Input(UiInput::Text(text)));
        }
    }
}

impl ApplicationHandler<AppEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.start(event_loop) {
            self.error = Some(e);
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.send_resize(),
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.pads.release_keyboard();
                }
                self.ui(UiMsg::Input(UiInput::Focus(focused)));
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => self.on_key(event),
            WindowEvent::CursorMoved { position, .. } => {
                let s = self.scale();
                self.pointer = egui::pos2(position.x as f32 / s, position.y as f32 / s);
                self.ui(UiMsg::Input(UiInput::PointerMoved(self.pointer)));
            }
            WindowEvent::CursorLeft { .. } => self.ui(UiMsg::Input(UiInput::PointerGone)),
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => egui::PointerButton::Primary,
                    MouseButton::Right => egui::PointerButton::Secondary,
                    MouseButton::Middle => egui::PointerButton::Middle,
                    _ => return,
                };
                self.ui(UiMsg::Input(UiInput::PointerButton {
                    pos: self.pointer,
                    button,
                    pressed: state == ElementState::Pressed,
                }));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta, unit) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (egui::vec2(x, y), egui::MouseWheelUnit::Line)
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = self.scale();
                        (
                            egui::vec2(p.x as f32 / s, p.y as f32 / s),
                            egui::MouseWheelUnit::Point,
                        )
                    }
                };
                self.ui(UiMsg::Input(UiInput::Scroll { delta, unit }));
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Pad(ev) => {
                if self.pads.handle_event(ev, &self.shared) {
                    self.ui(UiMsg::Gamepads(self.pads.names()));
                }
            }
            AppEvent::Main(msg) => match msg {
                MainMsg::SetFullscreen(on) => self.set_fullscreen(on),
                MainMsg::ToggleFullscreen => self.set_fullscreen(!self.is_fullscreen()),
                MainMsg::TextInput(on) => self.text_input(on),
                MainMsg::ShowCursor(on) => {
                    if let Some(w) = &self.window {
                        w.set_cursor_visible(on);
                    }
                }
                MainMsg::PressNexus => self.pads.pulse_nexus(),
                MainMsg::Quit => event_loop.exit(),
            },
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let streaming = self.shared.is_streaming();
        let mut wake: Option<Instant> = None;
        if let Some(t) = &mut self.test_input {
            if streaming && t.at.elapsed() > Duration::from_secs(1) {
                t.at = Instant::now();
                t.down = !t.down;
                self.pads.handle_key(t.key, t.down);
            }
            wake = Some(Instant::now() + Duration::from_millis(100));
        }
        // Push controller state to the stream (or drive the UI) once per
        // batch of events.
        self.pads.flush(&self.shared, &self.ui_tx);

        let next = match (wake, self.pads.next_tick()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        event_loop.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.ui(UiMsg::Quit);
        if let Some(t) = self.ui_thread.take() {
            let _ = t.join();
        }
    }
}

fn main() -> Result<()> {
    init_logging();
    let paths = Paths::new();
    let settings = Settings::load(&paths.settings());
    tracing::info!(config = %paths.config_dir.display(), "starting Nimbus");

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    let (ui_tx, ui_rx) = unbounded::<UiMsg>();
    let test_input = std::env::var("NIMBUS_TEST_INPUT").ok().map(|k| TestInput {
        key: if k == "y" {
            KeyCode::KeyY
        } else {
            KeyCode::Enter
        },
        at: Instant::now(),
        down: false,
    });
    let mut shell = Shell {
        shared: Arc::new(Shared::new(settings.deadzone)),
        settings,
        paths,
        proxy: event_loop.create_proxy(),
        ui_tx,
        ui_rx: Some(ui_rx),
        window: None,
        ui_thread: None,
        pads: Gamepads::new(),
        modifiers: ModifiersState::empty(),
        pointer: egui::Pos2::ZERO,
        test_input,
        error: None,
    };
    event_loop.run_app(&mut shell)?;
    // On platforms where `run_app` returns: make sure the UI thread is gone
    // before the window is dropped.
    if let Some(t) = shell.ui_thread.take() {
        let _ = shell.ui_tx.send(UiMsg::Quit);
        let _ = t.join();
    }
    match shell.error.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
