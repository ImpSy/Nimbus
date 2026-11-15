//! The UI / render thread.
//!
//! Owns wgpu + egui + the app state. Rendering is event driven: a frame is
//! drawn when a new video picture is decoded, when input arrives or when egui
//! asks for it — never on a fixed timer while streaming, so presenting never
//! delays the next video frame.

pub mod app;
pub mod images;
pub mod style;
pub mod views;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, select};
use egui::{Event, FocusDirection, Key, Modifiers, MouseWheelSource, TouchPhase};

use self::app::App;
use crate::pipeline::DecodedPicture;
use crate::render::Renderer;
use crate::settings::{Paths, Settings};
use crate::shared::{MainMsg, MainTx, NavAction, Shared, UiInput, UiMsg};

pub struct UiInit {
    pub instance: wgpu::Instance,
    pub surface: wgpu::Surface<'static>,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub ui_rx: Receiver<UiMsg>,
    pub main_tx: MainTx,
    pub shared: Arc<Shared>,
    pub settings: Settings,
    pub paths: Paths,
}

fn convert(input: UiInput, modifiers: &mut Modifiers) -> Option<Event> {
    Some(match input {
        UiInput::PointerMoved(p) => Event::PointerMoved(p),
        UiInput::PointerButton {
            pos,
            button,
            pressed,
        } => Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: *modifiers,
        },
        UiInput::PointerGone => Event::PointerGone,
        UiInput::Scroll { delta, unit } => Event::MouseWheel {
            unit,
            delta,
            phase: TouchPhase::Move,
            source: MouseWheelSource::Unknown,
            modifiers: *modifiers,
        },
        UiInput::Key {
            key,
            pressed,
            modifiers: m,
        } => {
            *modifiers = m;
            Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers: m,
            }
        }
        UiInput::Text(t) => Event::Text(t),
        UiInput::Focus(f) => Event::WindowFocused(f),
    })
}

fn key_tap(events: &mut Vec<Event>, key: Key) {
    for pressed in [true, false] {
        events.push(Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
    }
}

/// Controller navigation: directional moves are applied inside the next UI
/// pass (egui resolves focus moves at the end of a pass); activate / back
/// become key taps.
fn apply_nav(app: &mut App, nav: NavAction, events: &mut Vec<Event>) {
    let dir = match nav {
        NavAction::Up => Some(FocusDirection::Up),
        NavAction::Down => Some(FocusDirection::Down),
        NavAction::Left => Some(FocusDirection::Left),
        NavAction::Right => Some(FocusDirection::Right),
        _ => None,
    };
    if let Some(dir) = dir {
        app.pending_focus_moves.push(dir);
        return;
    }
    match nav {
        NavAction::Activate => key_tap(events, Key::Enter),
        NavAction::Back => key_tap(events, Key::Escape),
        _ => {}
    }
}

fn timeout_hint(next: &mut Option<Instant>, at: Instant) {
    if next.is_none_or(|n| n > at) {
        *next = Some(at);
    }
}

pub fn run(init: UiInit) -> anyhow::Result<()> {
    let mut renderer = Renderer::new(
        &init.instance,
        init.surface,
        init.width,
        init.height,
        init.settings.present,
    )?;
    let (frame_tx, frame_rx) = bounded::<()>(1);
    let (wake_tx, wake_rx) = bounded::<()>(1);

    let ctx = egui::Context::default();
    style::apply(&ctx);
    ctx.options_mut(|o| o.zoom_with_keyboard = false);

    let mut app = App::new(
        init.paths,
        init.settings,
        init.shared.clone(),
        init.main_tx.clone(),
        frame_tx,
        wake_tx,
    );
    app.renderer_info = format!("{} · {:?}", renderer.adapter_name, renderer.present_mode());

    let ui_rx = init.ui_rx;
    let mut scale = init.scale.max(0.5);
    let start = Instant::now();
    let mut events: Vec<Event> = Vec::new();
    let mut modifiers = Modifiers::default();
    let mut navs: Vec<NavAction> = Vec::new();
    let mut next_repaint: Option<Instant> = Some(Instant::now());
    let mut pending_pic: Option<DecodedPicture> = None;
    let mut text_input_active = false;
    let mut present_pref = app.settings.present;
    // Debug automation: NIMBUS_SCREENSHOT=<path>[,<seconds>] saves a PNG of the
    // window after the given delay; NIMBUS_EXIT_AFTER=<seconds> quits.
    let mut screenshot: Option<(std::path::PathBuf, Instant)> =
        std::env::var("NIMBUS_SCREENSHOT").ok().map(|v| {
            let mut it = v.splitn(2, ',');
            let path = std::path::PathBuf::from(it.next().unwrap_or("nimbus.png"));
            let secs: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(3.0);
            (path, Instant::now() + Duration::from_secs_f32(secs))
        });
    // Debug automation: NIMBUS_TEST_NAV=down,down,right,activate replays
    // controller navigation (one action every 400 ms after 1.5 s).
    let mut test_nav: std::collections::VecDeque<NavAction> = std::env::var("NIMBUS_TEST_NAV")
        .unwrap_or_default()
        .split(',')
        .filter_map(|a| match a.trim() {
            "up" => Some(NavAction::Up),
            "down" => Some(NavAction::Down),
            "left" => Some(NavAction::Left),
            "right" => Some(NavAction::Right),
            "activate" | "a" => Some(NavAction::Activate),
            "back" | "b" => Some(NavAction::Back),
            "lb" => Some(NavAction::PrevTab),
            "rb" => Some(NavAction::NextTab),
            "wait" => Some(NavAction::Secondary),
            _ => None,
        })
        .collect();
    let mut test_nav_at = Instant::now() + Duration::from_millis(1500);
    let exit_at = std::env::var("NIMBUS_EXIT_AFTER")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .map(|s| Instant::now() + Duration::from_secs_f32(s));

    'outer: loop {
        let now = Instant::now();
        let mut timeout = next_repaint
            .map(|t| t.saturating_duration_since(now))
            .unwrap_or(Duration::from_secs(1));
        if app.stream.is_some() {
            timeout = timeout.min(Duration::from_millis(20));
        }

        let mut woke = false;
        let mut msgs = Vec::new();
        select! {
            recv(ui_rx) -> m => match m {
                Ok(m) => msgs.push(m),
                Err(_) => break 'outer,
            },
            recv(frame_rx) -> _ => woke = true,
            recv(wake_rx) -> _ => woke = true,
            default(timeout) => {},
        }
        msgs.extend(ui_rx.try_iter());

        for m in msgs {
            match m {
                UiMsg::Input(i) => {
                    if let Some(e) = convert(i, &mut modifiers) {
                        events.push(e);
                    }
                }
                UiMsg::Resize {
                    width,
                    height,
                    scale: s,
                } => {
                    renderer.resize(width, height);
                    scale = s.max(0.5);
                    woke = true;
                }
                UiMsg::Nav(n) => navs.push(n),
                UiMsg::ToggleMenu => {
                    if app.is_streaming() {
                        app.toggle_menu();
                    }
                    woke = true;
                }
                UiMsg::ToggleStats => {
                    app.show_stats = !app.show_stats;
                    woke = true;
                }
                UiMsg::Gamepads(g) => {
                    app.gamepads = g;
                    woke = true;
                }
                UiMsg::Quit => break 'outer,
            }
        }

        if !test_nav.is_empty() {
            if Instant::now() >= test_nav_at {
                if let Some(n) = test_nav.pop_front() {
                    // "wait" (mapped to Secondary) is a pause in the script.
                    if n != NavAction::Secondary {
                        tracing::info!(?n, "test nav");
                        navs.push(n);
                    }
                }
                test_nav_at = Instant::now() + Duration::from_millis(400);
            }
            timeout_hint(&mut next_repaint, test_nav_at);
        }
        app.update(&ctx);
        if app.quit || exit_at.is_some_and(|t| Instant::now() >= t) {
            break;
        }
        if let Some((path, at)) = &screenshot {
            if Instant::now() >= *at {
                renderer.request_screenshot(path.clone());
                screenshot = None;
                woke = true;
            } else {
                timeout_hint(&mut next_repaint, *at);
            }
        }
        if app.settings.present != present_pref {
            present_pref = app.settings.present;
            renderer.set_present(present_pref);
            app.renderer_info =
                format!("{} · {:?}", renderer.adapter_name, renderer.present_mode());
        }

        if let Some(pic) = app.take_picture() {
            renderer.upload(&pic.frame, app.settings.sharpness);
            pending_pic = Some(pic);
        } else if !app.is_streaming() && renderer.has_video() {
            renderer.clear_video();
        }
        renderer.set_sharpness(app.settings.sharpness);

        let due = next_repaint.is_some_and(|t| Instant::now() >= t);
        if !(woke || due || pending_pic.is_some() || !events.is_empty() || !navs.is_empty()) {
            continue;
        }

        for nav in navs.drain(..) {
            if !app.on_nav(nav) {
                apply_nav(&mut app, nav, &mut events);
            }
        }

        let (w, h) = renderer.size();
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(w as f32 / scale, h as f32 / scale),
            )),
            time: Some(start.elapsed().as_secs_f64()),
            events: std::mem::take(&mut events),
            focused: true,
            max_texture_side: Some(8192),
            ..Default::default()
        };
        if let Some(vp) = raw.viewports.get_mut(&egui::ViewportId::ROOT) {
            vp.native_pixels_per_point = Some(scale);
        } else {
            raw.viewports.insert(
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    native_pixels_per_point: Some(scale),
                    ..Default::default()
                },
            );
        }

        let output = ctx.run_ui(raw, |ui| views::draw(&mut app, ui));

        // Platform integration.
        let wants_text = output.platform_output.ime.is_some();
        if wants_text != text_input_active {
            text_input_active = wants_text;
            let _ = init.main_tx.send(MainMsg::TextInput(wants_text));
        }
        for cmd in &output.platform_output.commands {
            if let egui::OutputCommand::OpenUrl(u) = cmd {
                let _ = webbrowser::open(&u.url);
            }
        }
        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        next_repaint = (delay < Duration::from_secs(3600)).then(|| Instant::now() + delay);

        let ppp = output.pixels_per_point;
        let draw_video = app.is_streaming();
        if let Some(info) = renderer.render(draw_video, &ctx, output, ppp)
            && let Some(pic) = pending_pic.take()
        {
            app.on_presented(&pic, info.presented_at);
        }
        pending_pic = None;
    }

    tracing::info!("UI thread shutting down");
    app.shutdown();
    let _ = init.main_tx.send(MainMsg::Quit);
    Ok(())
}
