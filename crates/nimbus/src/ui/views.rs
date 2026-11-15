//! All screens. Designed to be fully usable with a controller (big targets,
//! visible focus, no combo boxes / sliders).

use std::time::Duration;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Sense, Stroke,
    StrokeKind, Ui, vec2,
};
use xcloud::device_info::Resolution;
use xcloud::stream::CodecPreference;

use super::app::{App, Load, LoginState, Screen, StreamTarget, Tab};
use super::style::{ACCENT, ACCENT_BRIGHT, BG_CARD, BG_CARD_HOVER, TEXT_DIM};
use crate::settings::{DecoderPref, PresentPref};
use crate::shared::MainMsg;

pub fn draw(app: &mut App, ui: &mut Ui) {
    if app.request_focus_reset && app.focus_reset_frames == 0 {
        // Drop focus from the previous screen (once); this screen's first
        // widget claims it below.
        ui.ctx().memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }
    for dir in app.pending_focus_moves.drain(..) {
        ui.ctx().memory_mut(|m| {
            if m.focused().is_none() {
                m.move_focus(egui::FocusDirection::Next);
            } else {
                m.move_focus(dir);
            }
        });
    }
    match &app.screen {
        Screen::Startup => {
            ui.centered_and_justified(|ui| ui.spinner());
        }
        Screen::Login(_) => login(app, ui),
        Screen::Home => home(app, ui),
        Screen::Connecting(_) => connecting(app, ui),
        Screen::Streaming => streaming(app, ui),
    }
    toast(app, ui);
    // Keep offering focus until a widget actually took it (lists may still
    // be loading on the first frames after a screen change), for a while.
    if app.request_focus_reset {
        app.focus_reset_frames += 1;
        if ui.ctx().memory(|m| m.focused().is_some()) || app.focus_reset_frames > 600 {
            app.request_focus_reset = false;
        }
    }
    if !app.request_focus_reset {
        app.focus_reset_frames = 0;
    }
}

fn big_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(20.0).strong())
            .min_size(vec2(260.0, 52.0))
            .corner_radius(10.0),
    )
}

// ---------------------------------------------------------------- login

fn login(app: &mut App, ui: &mut Ui) {
    let mut action_login = false;
    let mut action_cancel = false;
    let mut open_url = None;
    let focus_reset = app.request_focus_reset;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.12).max(20.0));
        ui.label(RichText::new("NIMBUS").size(56.0).strong().color(ACCENT_BRIGHT));
        ui.label(
            RichText::new("Native Xbox remote play & cloud gaming")
                .size(18.0)
                .color(TEXT_DIM),
        );
        ui.add_space(40.0);
        let Screen::Login(state) = &app.screen else { return };
        match state {
            LoginState::Idle => {
                let b = big_button(ui, "Sign in with Microsoft");
                if focus_reset {
                    b.request_focus();
                }
                if b.clicked() {
                    action_login = true;
                }
            }
            LoginState::Requesting => {
                ui.spinner();
                ui.label("Contacting Microsoft…");
            }
            LoginState::Code { code, qr } => {
                ui.label(RichText::new("On your phone or computer, go to").size(18.0));
                ui.label(
                    RichText::new("microsoft.com/link")
                        .size(28.0)
                        .strong()
                        .color(ACCENT_BRIGHT),
                );
                ui.label(RichText::new("and enter this code").size(18.0));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(&code.user_code)
                        .font(FontId::monospace(44.0))
                        .strong(),
                );
                ui.add_space(12.0);
                if let Some(qr) = qr {
                    ui.add(egui::Image::new((qr.id(), vec2(190.0, 190.0))).corner_radius(8.0));
                    ui.label(RichText::new("or scan this QR code").color(TEXT_DIM));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let w = 2.0 * 220.0 + ui.spacing().item_spacing.x;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    let b = ui.add(egui::Button::new("Open in browser").min_size(vec2(220.0, 44.0)));
                    if focus_reset {
                        b.request_focus();
                    }
                    if b.clicked() {
                        open_url = Some(code.verification_uri_complete());
                    }
                    if ui
                        .add(egui::Button::new("Cancel").min_size(vec2(220.0, 44.0)))
                        .clicked()
                    {
                        action_cancel = true;
                    }
                });
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 220.0).max(0.0) / 2.0);
                    ui.spinner();
                    ui.label("Waiting for you to sign in…");
                });
            }
            LoginState::Error(e) => {
                ui.label(RichText::new(e).color(Color32::from_rgb(255, 120, 120)));
                ui.add_space(12.0);
                let b = big_button(ui, "Try again");
                if focus_reset {
                    b.request_focus();
                }
                if b.clicked() {
                    action_login = true;
                }
            }
        }
    });
    if let Some(url) = open_url {
        let _ = webbrowser::open(&url);
    }
    if action_login {
        app.begin_login();
        app.request_focus_reset = true;
    }
    if action_cancel {
        app.screen = Screen::Login(LoginState::Idle);
        app.request_focus_reset = true;
    }
}

// ----------------------------------------------------------------- home

fn home(app: &mut App, ui: &mut Ui) {
    egui::Panel::top("top")
        .frame(Frame::new().inner_margin(Margin::symmetric(24, 14)).fill(Color32::from_rgb(14, 16, 15)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("NIMBUS").size(26.0).strong().color(ACCENT_BRIGHT));
                ui.add_space(24.0);
                for tab in Tab::ALL {
                    let selected = app.tab == tab;
                    let r = ui.add(
                        egui::Button::new(RichText::new(tab.label()).size(18.0))
                            .selected(selected)
                            .min_size(vec2(150.0, 40.0))
                            .corner_radius(20.0),
                    );
                    if r.clicked() {
                        app.tab = tab;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(tag) = &app.gamertag {
                        ui.label(RichText::new(tag).size(18.0).strong());
                    }
                    let pads = app.gamepads.len();
                    let label = if pads == 0 {
                        "No controller".to_owned()
                    } else {
                        format!("{pads} controller{}", if pads > 1 { "s" } else { "" })
                    };
                    ui.label(RichText::new(label).color(TEXT_DIM))
                        .on_hover_text(
                            app.gamepads
                                .iter()
                                .map(|(_, n)| n.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                });
            });
        });
    egui::Panel::bottom("hints")
        .frame(Frame::new().inner_margin(Margin::symmetric(24, 8)).fill(Color32::from_rgb(14, 16, 15)))
        .show(ui, |ui| {
            ui.label(
                RichText::new("LB / RB  switch tabs      A  select      B  back      Y  refresh      View + Menu  in-game menu")
                    .size(14.0)
                    .color(TEXT_DIM),
            );
        });
    egui::CentralPanel::default()
        .frame(Frame::new().inner_margin(Margin::symmetric(24, 18)))
        .show(ui, |ui| match app.tab {
            Tab::Consoles => consoles(app, ui),
            Tab::Cloud => cloud(app, ui),
            Tab::Settings => settings(app, ui),
        });
}

fn section_title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(24.0).strong());
    ui.add_space(6.0);
}

fn consoles(app: &mut App, ui: &mut Ui) {
    ui.horizontal(|ui| {
        section_title(ui, "Remote play");
        let empty = !matches!(&app.consoles, Load::Loaded(l) if !l.is_empty());
        let focus = app.request_focus_reset && empty && !app.consoles.is_loading();
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let r = ui.button("Refresh");
            if focus {
                r.request_focus();
            }
            if r.clicked() {
                app.refresh_consoles();
            }
        });
    });
    let mut start = None;
    match &app.consoles {
        Load::NotLoaded | Load::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for your consoles…");
            });
        }
        Load::Failed(e) => {
            ui.colored_label(Color32::from_rgb(255, 120, 120), format!("Couldn't list consoles: {e}"));
        }
        Load::Loaded(list) if list.is_empty() => {
            ui.label("No console found for this account.");
            ui.label(
                RichText::new(
                    "On your Xbox: Settings › Devices & connections › Remote features › enable remote features.",
                )
                .color(TEXT_DIM),
            );
        }
        Load::Loaded(list) => {
            let focus_reset = app.request_focus_reset;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(18.0, 18.0);
                for (i, c) in list.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(340.0, 190.0), Sense::click());
                    if i == 0 && focus_reset {
                        resp.request_focus();
                    }
                    let focused = resp.has_focus() || resp.hovered();
                    let p = ui.painter();
                    p.rect_filled(rect, 14.0, if focused { BG_CARD_HOVER } else { BG_CARD });
                    if resp.has_focus() {
                        p.rect_stroke(rect, 14.0, Stroke::new(3.0, ACCENT_BRIGHT), StrokeKind::Inside);
                    }
                    let x = rect.left() + 22.0;
                    p.text(
                        egui::pos2(x, rect.top() + 22.0),
                        Align2::LEFT_TOP,
                        &c.device_name,
                        FontId::proportional(26.0),
                        Color32::WHITE,
                    );
                    p.text(
                        egui::pos2(x, rect.top() + 60.0),
                        Align2::LEFT_TOP,
                        c.pretty_type(),
                        FontId::proportional(17.0),
                        TEXT_DIM,
                    );
                    let (dot, state) = if c.is_on() {
                        (ACCENT_BRIGHT, "On")
                    } else if c.power_state.eq_ignore_ascii_case("ConnectedStandby") {
                        (Color32::from_rgb(240, 190, 60), "Sleeping (will wake up)")
                    } else {
                        (Color32::from_rgb(140, 140, 140), c.power_state.as_str())
                    };
                    p.circle_filled(egui::pos2(x + 6.0, rect.top() + 100.0), 6.0, dot);
                    p.text(
                        egui::pos2(x + 20.0, rect.top() + 100.0),
                        Align2::LEFT_CENTER,
                        state,
                        FontId::proportional(16.0),
                        Color32::from_gray(210),
                    );
                    let btn = egui::Rect::from_min_size(
                        egui::pos2(x, rect.bottom() - 56.0),
                        vec2(rect.width() - 44.0, 38.0),
                    );
                    p.rect_filled(btn, 19.0, if focused { ACCENT_BRIGHT } else { ACCENT });
                    p.text(
                        btn.center(),
                        Align2::CENTER_CENTER,
                        "Play remotely",
                        FontId::proportional(18.0),
                        Color32::WHITE,
                    );
                    if resp.clicked() {
                        start = Some(StreamTarget::Home {
                            server_id: c.server_id.clone(),
                            name: c.device_name.clone(),
                        });
                    }
                }
            });
        }
    }
    if let Some(t) = start {
        app.start_stream(t);
    }
}

fn game_tile(app: &mut App, ui: &mut Ui, game: &super::app::CloudGame, focus: bool) -> egui::Response {
    let size = vec2(178.0, 236.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if focus {
        resp.request_focus();
    }
    if resp.gained_focus() {
        resp.scroll_to_me(Some(Align::Center));
    }
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let focused = resp.has_focus() || resp.hovered();
    let img_rect = egui::Rect::from_min_size(rect.min, vec2(size.x, size.x));
    let p = ui.painter();
    let tex = game.image.as_deref().and_then(|u| app.images.get(u));
    match tex {
        Some(id) => {
            let mut mesh_rect = img_rect;
            if focused {
                mesh_rect = mesh_rect.expand(3.0);
            }
            p.rect_filled(mesh_rect, 12.0, BG_CARD);
            egui::Image::new((id, mesh_rect.size()))
                .corner_radius(12.0)
                .paint_at(ui, mesh_rect);
        }
        None => {
            p.rect_filled(img_rect, 12.0, BG_CARD);
            let initials: String = game
                .name
                .split_whitespace()
                .filter_map(|w| w.chars().next())
                .take(2)
                .collect();
            p.text(
                img_rect.center(),
                Align2::CENTER_CENTER,
                initials,
                FontId::proportional(42.0),
                TEXT_DIM,
            );
        }
    }
    if resp.has_focus() {
        p.rect_stroke(img_rect.expand(3.0), 14.0, Stroke::new(3.0, ACCENT_BRIGHT), StrokeKind::Outside);
    }
    let resp = if game.publisher.is_empty() {
        resp
    } else {
        resp.on_hover_text(&game.publisher)
    };
    if !game.playable {
        let badge = egui::Rect::from_min_size(img_rect.left_top() + vec2(8.0, 8.0), vec2(96.0, 22.0));
        p.rect_filled(badge, 11.0, Color32::from_black_alpha(200));
        p.text(badge.center(), Align2::CENTER_CENTER, "Not owned", FontId::proportional(13.0), Color32::WHITE);
    }
    let text_rect = egui::Rect::from_min_max(egui::pos2(rect.left(), img_rect.bottom() + 6.0), rect.max);
    let galley = ui.painter().layout(
        game.name.clone(),
        FontId::proportional(15.0),
        if focused { Color32::WHITE } else { Color32::from_gray(200) },
        text_rect.width(),
    );
    ui.painter()
        .with_clip_rect(text_rect)
        .galley(text_rect.min, galley, Color32::WHITE);
    resp
}

fn cloud(app: &mut App, ui: &mut Ui) {
    let lib = match &app.cloud {
        Load::NotLoaded | Load::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading the cloud library…");
            });
            return;
        }
        Load::Failed(e) => {
            ui.label(RichText::new(e).color(Color32::from_rgb(255, 120, 120)));
            if ui.button("Retry").clicked() {
                app.refresh_cloud();
            }
            return;
        }
        Load::Loaded(lib) => lib,
    };
    let offering = match lib.offering {
        xcloud::auth::Offering::XCloudF2p => "Free-to-play",
        _ => "Game Pass Ultimate",
    };
    let region = lib.region.clone();
    let query = app.search.to_lowercase();
    let show_all = app.show_all_games;
    let games: Vec<_> = lib
        .games
        .iter()
        .filter(|g| show_all || g.playable)
        .filter(|g| query.is_empty() || g.name.to_lowercase().contains(&query))
        .cloned()
        .collect();
    let recent: Vec<_> = if query.is_empty() {
        lib.recent
            .iter()
            .filter_map(|id| lib.games.iter().find(|g| &g.title_id == id))
            .take(10)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let total = lib.games.len();

    ui.horizontal(|ui| {
        section_title(ui, "Cloud gaming");
        ui.label(RichText::new(format!("{offering} · {region} · {total} games")).color(TEXT_DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.checkbox(&mut app.show_all_games, "Show games I don't own");
            ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text("Search")
                    .desired_width(240.0),
            );
        });
    });
    ui.add_space(6.0);

    let mut start = None;
    let mut first = app.request_focus_reset;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        if !recent.is_empty() {
            ui.label(RichText::new("Jump back in").size(19.0).strong());
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
                for g in &recent {
                    if game_tile(app, ui, g, std::mem::take(&mut first)).clicked() {
                        start = Some(g.clone());
                    }
                }
            });
            ui.add_space(14.0);
            ui.label(RichText::new("Library").size(19.0).strong());
            ui.add_space(4.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(16.0, 16.0);
            for g in &games {
                if game_tile(app, ui, g, std::mem::take(&mut first)).clicked() {
                    start = Some(g.clone());
                }
            }
        });
        if games.is_empty() {
            ui.label(RichText::new("No game matches.").color(TEXT_DIM));
        }
    });
    if let Some(g) = start {
        if g.playable {
            app.start_stream(StreamTarget::Cloud {
                title_id: g.title_id,
                name: g.name,
            });
        } else {
            app.toast(format!("{} isn't included in your subscription.", g.name));
        }
    }
}

/// A row of mutually-exclusive options (controller friendly).
fn choice<T: PartialEq + Copy>(ui: &mut Ui, label: &str, value: &mut T, options: &[(T, &str)]) -> bool {
    choice_focus(ui, label, value, options, false)
}

fn choice_focus<T: PartialEq + Copy>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    options: &[(T, &str)],
    focus_selected: bool,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(220.0, 36.0), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(17.0),
            Color32::from_gray(200),
        );
        for (v, text) in options {
            let r = ui.add(
                egui::Button::new(*text)
                    .selected(*value == *v)
                    .min_size(vec2(0.0, 36.0))
                    .corner_radius(18.0),
            );
            if focus_selected && *value == *v {
                r.request_focus();
            }
            if r.clicked() && *value != *v {
                *value = *v;
                changed = true;
            }
        }
    });
    changed
}

fn settings(app: &mut App, ui: &mut Ui) {
    let mut changed = false;
    let mut region_changed = false;
    let mut sign_out = false;
    let focus_reset = app.request_focus_reset;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        let s = &mut app.settings;
        section_title(ui, "Stream quality");
        let res: Vec<(Resolution, &str)> = Resolution::ALL.iter().map(|r| (*r, r.label())).collect();
        changed |= choice_focus(ui, "Resolution", &mut s.resolution, &res, focus_reset);
        let codecs: Vec<(CodecPreference, &str)> =
            CodecPreference::ALL.iter().map(|c| (*c, c.label())).collect();
        changed |= choice(ui, "H.264 profile", &mut s.codec, &codecs);
        changed |= choice(
            ui,
            "Max bitrate",
            &mut s.max_bitrate_mbps,
            &[(0, "Unlimited"), (40, "40 Mbps"), (25, "25 Mbps"), (15, "15 Mbps"), (8, "8 Mbps")],
        );
        let regions: Vec<String> = match &app.cloud {
            Load::Loaded(lib) if lib.regions.len() > 1 => lib.regions.clone(),
            _ => Vec::new(),
        };
        if !regions.is_empty() {
            let s = &mut app.settings;
            let mut opts: Vec<(Option<&str>, &str)> = vec![(None, "Automatic")];
            opts.extend(regions.iter().map(|r| (Some(r.as_str()), r.as_str())));
            let mut current = s.xcloud_region.as_deref();
            if choice(ui, "Cloud region", &mut current, &opts) {
                s.xcloud_region = current.map(str::to_owned);
                changed = true;
                region_changed = true;
            }
        }
        let s = &mut app.settings;
        ui.add_space(14.0);
        section_title(ui, "Video");
        let decoders: Vec<(DecoderPref, &str)> = DecoderPref::ALL.iter().map(|d| (*d, d.label())).collect();
        changed |= choice(ui, "Decoder", &mut s.decoder, &decoders);
        let presents: Vec<(PresentPref, &str)> = PresentPref::ALL.iter().map(|p| (*p, p.label())).collect();
        changed |= choice(ui, "Presentation", &mut s.present, &presents);
        let mut sharp = (s.sharpness * 100.0).round() as u32;
        if choice(
            ui,
            "Sharpening",
            &mut sharp,
            &[(0, "Off"), (20, "Low"), (35, "Medium"), (60, "High"), (90, "Max")],
        ) {
            s.sharpness = sharp as f32 / 100.0;
            changed = true;
        }
        changed |= choice(ui, "Fullscreen while playing", &mut s.fullscreen_on_stream, &[(true, "On"), (false, "Off")]);
        changed |= choice(ui, "Statistics overlay", &mut s.show_stats, &[(true, "On"), (false, "Off")]);
        ui.add_space(14.0);
        section_title(ui, "Controls & audio");
        let mut dz = (s.deadzone * 100.0).round() as u32;
        if choice(ui, "Stick deadzone", &mut dz, &[(0, "0%"), (5, "5%"), (8, "8%"), (12, "12%"), (18, "18%")]) {
            s.deadzone = dz as f32 / 100.0;
            changed = true;
        }
        changed |= choice(ui, "Vibration", &mut s.vibration, &[(true, "On"), (false, "Off")]);
        changed |= choice(
            ui,
            "Audio buffer",
            &mut s.audio_buffer_ms,
            &[(20, "20 ms"), (40, "40 ms"), (60, "60 ms"), (100, "100 ms")],
        );
        ui.add_space(14.0);
        section_title(ui, "Account");
        if app.mock.is_some() {
            ui.label("Mock mode (no account).");
        } else if ui.add(egui::Button::new("Sign out").min_size(vec2(160.0, 38.0))).clicked() {
            sign_out = true;
        }
        ui.add_space(14.0);
        ui.label(
            RichText::new(format!(
                "Nimbus {} · GPU: {}",
                env!("CARGO_PKG_VERSION"),
                app.renderer_info
            ))
            .color(TEXT_DIM),
        );
        ui.label(
            RichText::new("Changes to quality settings apply to the next stream.")
                .color(TEXT_DIM),
        );
    });
    if changed {
        app.show_stats = app.settings.show_stats;
        app.save_settings();
    }
    if region_changed {
        app.cloud = Load::NotLoaded;
        app.refresh_cloud();
    }
    if sign_out {
        app.sign_out();
    }
}

// ----------------------------------------------------------- connecting

fn connecting(app: &mut App, ui: &mut Ui) {
    let Screen::Connecting(c) = &app.screen else { return };
    let mut cancel = false;
    let focus_reset = app.request_focus_reset;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.28);
        ui.label(RichText::new(&c.name).size(38.0).strong());
        ui.add_space(18.0);
        ui.add(egui::Spinner::new().size(44.0).color(ACCENT_BRIGHT));
        ui.add_space(14.0);
        ui.label(RichText::new(&c.status).size(20.0));
        if let Some(w) = c.wait_seconds.filter(|w| *w > 0) {
            ui.label(
                RichText::new(format!("Estimated wait: {} min", (w + 59) / 60))
                    .color(TEXT_DIM),
            );
        }
        ui.label(
            RichText::new(format!("{} s", c.started.elapsed().as_secs()))
                .color(TEXT_DIM),
        );
        ui.add_space(24.0);
        let b = ui.add(egui::Button::new("Cancel").min_size(vec2(200.0, 44.0)));
        if focus_reset {
            b.request_focus();
        }
        if b.clicked() {
            cancel = true;
        }
    });
    ui.ctx().request_repaint_after(Duration::from_millis(250));
    if cancel {
        app.end_stream(Some("Cancelled".into()));
    }
}

// ------------------------------------------------------------ streaming

fn streaming(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let Some(s) = app.stream.as_ref() else { return };
    let screen = ctx.content_rect();

    if s.first_frame.is_none() {
        egui::Area::new(egui::Id::unique("waiting"))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(&ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(&s.name).size(30.0).strong());
                    ui.add(egui::Spinner::new().size(36.0).color(ACCENT_BRIGHT));
                    ui.label(if s.ready { "Waiting for video…" } else { "Connecting…" });
                });
            });
        ctx.request_repaint_after(Duration::from_millis(100));
    } else if s
        .first_frame
        .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        && !s.menu_open
    {
        egui::Area::new(egui::Id::unique("hint"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -40.0))
            .show(&ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_black_alpha(170))
                    .corner_radius(20.0)
                    .inner_margin(Margin::symmetric(18, 10))
                    .show(ui, |ui| {
                        ui.label("View + Menu (or Esc) opens the menu  ·  F1 stats  ·  F11 fullscreen");
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    if app.show_stats {
        stats_overlay(app, &ctx);
    }

    let mut action: Option<&'static str> = None;
    let mut dialog_answer = None;
    let s = app.stream.as_ref().unwrap();
    if let Some(d) = &s.dialog {
        ctx.layer_painter(egui::LayerId::background())
            .rect_filled(screen, 0.0, Color32::from_black_alpha(140));
        egui::Area::new(egui::Id::unique("dialog"))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(&ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(24, 27, 26))
                    .corner_radius(16.0)
                    .inner_margin(Margin::same(28))
                    .show(ui, |ui| {
                        ui.set_max_width(520.0);
                        ui.label(RichText::new(&d.title).size(24.0).strong());
                        ui.add_space(8.0);
                        ui.label(&d.text);
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            for (i, b) in d.buttons.iter().enumerate() {
                                let r = ui.add(egui::Button::new(b).min_size(vec2(140.0, 40.0)));
                                if i == 0 && app.request_focus_reset {
                                    r.request_focus();
                                }
                                if r.clicked() {
                                    dialog_answer = Some(i);
                                }
                            }
                        });
                    });
            });
    } else if s.menu_open {
        ctx.layer_painter(egui::LayerId::background())
            .rect_filled(screen, 0.0, Color32::from_black_alpha(150));
        egui::Area::new(egui::Id::unique("menu"))
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(&ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(24, 27, 26))
                    .corner_radius(18.0)
                    .inner_margin(Margin::same(28))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(&s.name).size(26.0).strong());
                            if let Some(t) = &s.title_info {
                                ui.label(RichText::new(t).color(TEXT_DIM));
                            }
                            ui.add_space(14.0);
                            let resume = big_button(ui, "Resume");
                            if app.request_focus_reset {
                                resume.request_focus();
                            }
                            if resume.clicked() {
                                action = Some("resume");
                            }
                            if big_button(ui, "Xbox button").clicked() {
                                action = Some("nexus");
                            }
                            if big_button(ui, if app.show_stats { "Hide statistics" } else { "Show statistics" }).clicked() {
                                action = Some("stats");
                            }
                            if big_button(ui, "Toggle fullscreen").clicked() {
                                action = Some("fullscreen");
                            }
                            if big_button(ui, "Request keyframe").clicked() {
                                action = Some("keyframe");
                            }
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("Disconnect").size(20.0).strong())
                                        .fill(Color32::from_rgb(150, 40, 40))
                                        .min_size(vec2(260.0, 52.0))
                                        .corner_radius(10.0),
                                )
                                .clicked()
                            {
                                action = Some("disconnect");
                            }
                        });
                    });
            });
    }

    if let Some(i) = dialog_answer {
        app.answer_dialog(i);
    }
    match action {
        Some("resume") => app.toggle_menu(),
        Some("nexus") => {
            app.toggle_menu();
            let _ = app.main_tx.send(MainMsg::PressNexus);
        }
        Some("stats") => app.show_stats = !app.show_stats,
        Some("fullscreen") => {
            let _ = app.main_tx.send(MainMsg::ToggleFullscreen);
        }
        Some("keyframe") => {
            if let Some(sess) = app.stream.as_ref().and_then(|s| s.session.as_ref()) {
                sess.handle.send(xcloud::stream::Command::RequestKeyframe);
            }
        }
        Some("disconnect") => app.end_stream(Some("Disconnected".into())),
        _ => {}
    }
}

fn stats_overlay(app: &App, ctx: &egui::Context) {
    let Some(s) = app.stream.as_ref() else { return };
    let d = &s.decode;
    let n = &s.net;
    let mut lines = vec![
        format!("{}x{} @ {:.0} fps   {}", d.width, d.height, s.fps, if d.backend.is_empty() { "…" } else { d.backend }),
        format!(
            "latency {:>5.1} ms = net {:.1} + decode {:.1} + render {:.1}",
            s.latency_ms, s.queue_ms, d.decode_ms_avg, s.render_ms
        ),
        format!(
            "bitrate {:>5.1} Mb/s   audio {:.0} kb/s",
            n.video_kbps / 1000.0,
            n.audio_kbps
        ),
        format!(
            "rtt     {}   jitter {:.1} ms",
            n.rtt_ms.map(|r| format!("{r:>5.1} ms")).unwrap_or_else(|| "    -".into()),
            n.jitter_ms
        ),
        format!(
            "loss    {:.2}%   nack {}   pli {}",
            n.loss_percent, n.nacks_total, n.plis_total
        ),
        format!(
            "frames  {} decoded · {} skipped · {} hidden",
            d.decoded_frames, d.skipped_frames, d.dropped_for_keyframe
        ),
    ];
    if let Some((w, h)) = s.server_video_size {
        lines.push(format!("server  {w}x{h}"));
    }
    lines.push(format!("gpu     {}", app.renderer_info));
    egui::Area::new(egui::Id::unique("stats"))
        .anchor(Align2::LEFT_TOP, vec2(12.0, 12.0))
        .interactable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(Color32::from_black_alpha(165))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    for l in lines {
                        ui.label(
                            RichText::new(l)
                                .font(FontId::monospace(13.0))
                                .color(Color32::from_gray(230)),
                        );
                    }
                });
        });
    ctx.request_repaint_after(Duration::from_millis(500));
}

fn toast(app: &App, ui: &mut Ui) {
    let Some((msg, _)) = &app.toast else { return };
    egui::Area::new(egui::Id::unique("toast"))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 72.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(Color32::from_rgb(34, 38, 36))
                .stroke(Stroke::new(1.0, ACCENT))
                .corner_radius(12.0)
                .inner_margin(Margin::symmetric(18, 12))
                .show(ui, |ui| {
                    ui.label(RichText::new(msg).size(16.0));
                });
        });
    ui.ctx().request_repaint_after(Duration::from_millis(500));
}
