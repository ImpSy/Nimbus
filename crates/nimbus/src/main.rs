//! Nimbus — native, low-latency Xbox Remote Play & Cloud Gaming client.
//!
//! Thread layout:
//! * **main**: SDL3 event pump (window, keyboard, mouse, controllers, rumble).
//!   Controller state goes straight to the network thread.
//! * **ui**: egui + wgpu rendering and the app state machine.
//! * **xcloud-net**: WebRTC (str0m) + data-channel protocols.
//! * **video-decode** / **audio**: media decoding.
//! * tokio workers: HTTP APIs.

mod gamepad;
mod pipeline;
mod render;
mod settings;
mod shared;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::unbounded;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::{Keycode, Mod};
use sdl3::mouse::MouseButton;

use crate::gamepad::Gamepads;
use crate::render::SendDisplayHandle;
use crate::settings::{Paths, Settings};
use crate::shared::{MainMsg, Shared, UiInput, UiMsg};

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,str0m=warn,is=warn")
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn egui_key(k: Keycode) -> Option<egui::Key> {
    use egui::Key as E;
    use sdl3::keyboard::Keycode as K;
    Some(match k {
        K::Up => E::ArrowUp,
        K::Down => E::ArrowDown,
        K::Left => E::ArrowLeft,
        K::Right => E::ArrowRight,
        K::Return | K::KpEnter => E::Enter,
        K::Escape => E::Escape,
        K::Tab => E::Tab,
        K::Backspace => E::Backspace,
        K::Delete => E::Delete,
        K::Home => E::Home,
        K::End => E::End,
        K::PageUp => E::PageUp,
        K::PageDown => E::PageDown,
        K::Space => E::Space,
        other => return E::from_name(&other.name()),
    })
}

fn egui_modifiers(m: Mod) -> egui::Modifiers {
    let alt = m.intersects(Mod::LALTMOD | Mod::RALTMOD);
    let ctrl = m.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
    let shift = m.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
    let mac_cmd = cfg!(target_os = "macos") && m.intersects(Mod::LGUIMOD | Mod::RGUIMOD);
    egui::Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd,
        command: if cfg!(target_os = "macos") {
            mac_cmd
        } else {
            ctrl
        },
    }
}

fn main() -> Result<()> {
    init_logging();
    let paths = Paths::new();
    let settings = Settings::load(&paths.settings());
    tracing::info!(config = %paths.config_dir.display(), "starting Nimbus");

    // Keep receiving controller input when the window is not focused (e.g.
    // Steam overlay), and use the positional (Xbox) button layout.
    // Automated / debug runs (screenshots, auto-exit, scripted input) stay
    // silent: route audio to SDL's dummy driver unless explicitly overridden.
    // NIMBUS_MUTE=1 does the same for interactive runs.
    let automated = [
        "NIMBUS_EXIT_AFTER",
        "NIMBUS_SCREENSHOT",
        "NIMBUS_TEST_INPUT",
        "NIMBUS_TEST_NAV",
        "NIMBUS_MUTE",
    ]
    .iter()
    .any(|v| std::env::var_os(v).is_some());
    if automated && std::env::var_os("SDL_AUDIO_DRIVER").is_none() {
        sdl3::hint::set("SDL_AUDIO_DRIVER", "dummy");
    }
    sdl3::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
    sdl3::hint::set("SDL_GAMECONTROLLER_USE_BUTTON_LABELS", "0");
    sdl3::hint::set("SDL_VIDEO_MINIMIZE_ON_FOCUS_LOSS", "0");

    let sdl = sdl3::init().context("SDL init")?;
    let video = sdl.video().context("SDL video")?;
    let gamepad_sys = sdl.gamepad().context("SDL gamepad")?;
    let _audio = sdl.audio().context("SDL audio")?;
    let mouse = sdl.mouse();

    let mut builder = video.window("Nimbus", 1280, 800);
    builder.position_centered().resizable().high_pixel_density();
    #[cfg(target_os = "macos")]
    builder.metal_view();
    #[cfg(not(target_os = "macos"))]
    builder.vulkan();
    let mut window = builder.build().context("create window")?;
    let _ = window.set_minimum_size(720, 450);
    if settings.start_fullscreen {
        let _ = window.set_fullscreen(true);
    }

    let scale_of = |w: &sdl3::video::Window| w.display_scale().max(0.5);
    // Window coordinates → egui points.
    let coord_factor = |w: &sdl3::video::Window| w.pixel_density() / scale_of(w);

    // wgpu instance + surface must be created on the main thread.
    let raw_display = window.display_handle()?.as_raw();
    let raw_window = window.window_handle()?.as_raw();
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
        Box::new(SendDisplayHandle(raw_display)),
    ));
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_display),
            raw_window_handle: raw_window,
        })
    }
    .context("create wgpu surface")?;

    let (ui_tx, ui_rx) = unbounded::<UiMsg>();
    let (main_tx, main_rx) = unbounded::<MainMsg>();
    let shared = Arc::new(Shared::new(settings.deadzone, settings.vibration));

    let (pw, ph) = window.size_in_pixels();
    let ui_thread = {
        let init = ui::UiInit {
            instance,
            surface,
            width: pw,
            height: ph,
            scale: scale_of(&window),
            ui_rx,
            main_tx: main_tx.clone(),
            shared: shared.clone(),
            settings: settings.clone(),
            paths: paths.clone(),
        };
        std::thread::Builder::new()
            .name("ui".into())
            .spawn(move || {
                if let Err(e) = ui::run(init) {
                    tracing::error!("UI thread failed: {e:#}");
                }
            })?
    };

    let mut pump = sdl.event_pump().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut pads = Gamepads::new(gamepad_sys);
    let mut evs = Vec::with_capacity(64);
    // Debug automation: press (A) every 2 s while streaming.
    let test_input = std::env::var("NIMBUS_TEST_INPUT").ok();
    let test_key = match test_input.as_deref() {
        Some("y") => Keycode::Y,
        _ => Keycode::Return,
    };
    let test_input = test_input.is_some();
    let mut test_input_at = std::time::Instant::now();
    let mut test_input_down = false;

    'main: loop {
        if ui_thread.is_finished() {
            tracing::error!("UI thread exited");
            break;
        }
        let streaming = shared.is_streaming();
        let timeout = if streaming || pads.needs_tick() {
            Duration::from_millis(2)
        } else {
            Duration::from_millis(12)
        };
        evs.clear();
        if let Some(e) = pump.wait_event_timeout(timeout) {
            evs.push(e);
            while let Some(e) = pump.poll_event() {
                evs.push(e);
            }
        }

        let factor = coord_factor(&window);
        let game_keys = shared.game_input().is_some();
        for ev in evs.drain(..) {
            match &ev {
                Event::Quit { .. } => break 'main,
                Event::Window { win_event, .. } => match win_event {
                    WindowEvent::PixelSizeChanged(..)
                    | WindowEvent::Resized(..)
                    | WindowEvent::DisplayChanged(..)
                    | WindowEvent::EnterFullscreen
                    | WindowEvent::LeaveFullscreen => {
                        let (w, h) = window.size_in_pixels();
                        let _ = ui_tx.send(UiMsg::Resize {
                            width: w,
                            height: h,
                            scale: scale_of(&window),
                        });
                    }
                    WindowEvent::FocusGained => {
                        let _ = ui_tx.send(UiMsg::Input(UiInput::Focus(true)));
                    }
                    WindowEvent::FocusLost => {
                        pads.release_keyboard();
                        let _ = ui_tx.send(UiMsg::Input(UiInput::Focus(false)));
                    }
                    WindowEvent::MouseLeave => {
                        let _ = ui_tx.send(UiMsg::Input(UiInput::PointerGone));
                    }
                    WindowEvent::CloseRequested => break 'main,
                    _ => {}
                },
                Event::MouseMotion { x, y, .. } => {
                    let _ = ui_tx.send(UiMsg::Input(UiInput::PointerMoved(egui::pos2(
                        x * factor,
                        y * factor,
                    ))));
                }
                Event::MouseButtonDown {
                    x, y, mouse_btn, ..
                }
                | Event::MouseButtonUp {
                    x, y, mouse_btn, ..
                } => {
                    let button = match mouse_btn {
                        MouseButton::Left => egui::PointerButton::Primary,
                        MouseButton::Right => egui::PointerButton::Secondary,
                        MouseButton::Middle => egui::PointerButton::Middle,
                        _ => continue,
                    };
                    let _ = ui_tx.send(UiMsg::Input(UiInput::PointerButton {
                        pos: egui::pos2(x * factor, y * factor),
                        button,
                        pressed: matches!(ev, Event::MouseButtonDown { .. }),
                    }));
                }
                Event::MouseWheel { x, y, .. } => {
                    let _ = ui_tx.send(UiMsg::Input(UiInput::Scroll(egui::vec2(*x, *y))));
                }
                Event::KeyDown {
                    keycode: Some(k),
                    keymod,
                    repeat,
                    ..
                }
                | Event::KeyUp {
                    keycode: Some(k),
                    keymod,
                    repeat,
                    ..
                } => {
                    let down = matches!(ev, Event::KeyDown { .. });
                    // Global hotkeys.
                    if down && !repeat {
                        match *k {
                            Keycode::F11 => {
                                let fs =
                                    window.fullscreen_state() != sdl3::video::FullscreenType::Off;
                                let _ = window.set_fullscreen(!fs);
                                continue;
                            }
                            Keycode::F1 => {
                                let _ = ui_tx.send(UiMsg::ToggleStats);
                                continue;
                            }
                            Keycode::Escape | Keycode::F10 if streaming => {
                                pads.release_keyboard();
                                pads.flush(&shared, &ui_tx);
                                let _ = ui_tx.send(UiMsg::ToggleMenu);
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if game_keys {
                        if !repeat {
                            pads.handle_key(*k, down);
                        }
                        continue;
                    }
                    if let Some(key) = egui_key(*k) {
                        let _ = ui_tx.send(UiMsg::Input(UiInput::Key {
                            key,
                            pressed: down,
                            modifiers: egui_modifiers(*keymod),
                        }));
                    }
                }
                Event::TextInput { text, .. } => {
                    if !game_keys {
                        let _ = ui_tx.send(UiMsg::Input(UiInput::Text(text.clone())));
                    }
                }
                Event::GamepadAdded { .. }
                | Event::GamepadRemoved { .. }
                | Event::GamepadButtonDown { .. }
                | Event::GamepadButtonUp { .. }
                | Event::GamepadAxisMotion { .. } => {
                    if pads.handle_event(&ev, &shared) {
                        let _ = ui_tx.send(UiMsg::Gamepads(pads.names()));
                    }
                }
                _ => {}
            }
        }
        if test_input && streaming && test_input_at.elapsed() > Duration::from_millis(1000) {
            test_input_at = std::time::Instant::now();
            test_input_down = !test_input_down;
            pads.handle_key(test_key, test_input_down);
        }
        // Push controller state to the stream (or drive the UI).
        pads.flush(&shared, &ui_tx);

        for msg in main_rx.try_iter() {
            match msg {
                MainMsg::Rumble(v) => {
                    if shared.vibration.load(std::sync::atomic::Ordering::Relaxed) {
                        pads.rumble(&v);
                    }
                }
                MainMsg::StopRumble => pads.stop_rumble(),
                MainMsg::SetFullscreen(on) => {
                    let _ = window.set_fullscreen(on);
                }
                MainMsg::ToggleFullscreen => {
                    let fs = window.fullscreen_state() != sdl3::video::FullscreenType::Off;
                    let _ = window.set_fullscreen(!fs);
                }
                MainMsg::TextInput(on) => {
                    let ti = video.text_input();
                    if on {
                        ti.start(&window);
                    } else {
                        ti.stop(&window);
                    }
                }
                MainMsg::ShowCursor(on) => mouse.show_cursor(on),
                MainMsg::PressNexus => pads.pulse_nexus(),
                MainMsg::Quit => break 'main,
            }
        }
    }

    let _ = ui_tx.send(UiMsg::Quit);
    let _ = ui_thread.join();
    pads.stop_rumble();
    Ok(())
}
