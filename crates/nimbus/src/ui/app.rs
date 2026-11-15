//! Application state machine (runs on the UI thread). All network work is
//! done on a tokio runtime and reported back through [`Backend`] messages.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use xcloud::account::Account;
use xcloud::api::{Console, GssvApi, SessionInfo, StreamKind};
use xcloud::auth::{Auth, DeviceCode, Offering};
use xcloud::protocol::input::FrameMetadata;
use xcloud::protocol::messages::Dialog;
use xcloud::stream::{
    Command, NetworkStats, SessionProgress, StreamConfig, StreamEvent, StreamSession, StreamSinks,
};

use crate::pipeline::{AudioPipeline, DecodeStats, DecodedPicture, VideoPipeline};
use crate::settings::{Paths, Settings};
use crate::shared::{MainMsg, NavAction, Shared};
use crate::ui::images::Images;

pub enum Load<T> {
    NotLoaded,
    Loading,
    Loaded(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn is_loading(&self) -> bool {
        matches!(self, Load::Loading)
    }
}

#[derive(Clone)]
pub struct CloudGame {
    pub title_id: String,
    pub name: String,
    pub publisher: String,
    pub image: Option<String>,
    pub playable: bool,
}

pub struct CloudLibrary {
    pub offering: Offering,
    pub region: String,
    /// All regions the account may use (for the settings picker).
    pub regions: Vec<String>,
    pub base_uri: String,
    pub games: Vec<CloudGame>,
    pub recent: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Consoles,
    Cloud,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Consoles, Tab::Cloud, Tab::Settings];
    pub fn label(self) -> &'static str {
        match self {
            Tab::Consoles => "My consoles",
            Tab::Cloud => "Cloud gaming",
            Tab::Settings => "Settings",
        }
    }
}

pub enum LoginState {
    Idle,
    Requesting,
    Code {
        code: DeviceCode,
        qr: Option<egui::TextureHandle>,
    },
    Error(String),
}

pub struct Connecting {
    pub name: String,
    pub status: String,
    pub started: Instant,
    pub wait_seconds: Option<i64>,
}

pub enum Screen {
    Startup,
    Login(LoginState),
    Home,
    Connecting(Connecting),
    Streaming,
}

pub enum Backend {
    DeviceCode(Result<DeviceCode, String>),
    LoggedIn(Result<Arc<Account>, String>),
    Profile(String),
    Consoles(Result<(String, Vec<Console>), String>),
    Cloud(Result<Option<CloudLibrary>, String>),
    StreamProgress(u64, SessionProgress),
    StreamStarted(u64, Result<StreamSession, String>),
    AuthExpired(String),
}

#[derive(Clone)]
pub enum StreamTarget {
    Home { server_id: String, name: String },
    Cloud { title_id: String, name: String },
}

impl StreamTarget {
    fn name(&self) -> &str {
        match self {
            StreamTarget::Home { name, .. } | StreamTarget::Cloud { name, .. } => name,
        }
    }
}

pub struct ActiveStream {
    pub id: u64,
    pub name: String,
    pub session: Option<StreamSession>,
    task: Option<tokio::task::JoinHandle<()>>,
    created: Arc<Mutex<Option<(GssvApi, SessionInfo)>>>,
    video: Option<VideoPipeline>,
    audio: Option<AudioPipeline>,
    events: Receiver<StreamEvent>,
    pub net: NetworkStats,
    pub decode: DecodeStats,
    pub ready: bool,
    pub menu_open: bool,
    pub dialog: Option<Dialog>,
    present_times: VecDeque<Instant>,
    pub fps: f32,
    pub latency_ms: f32,
    /// First packet → decoder input (frame assembly + queueing).
    pub queue_ms: f32,
    /// Decoded → presented (upload + render + present).
    pub render_ms: f32,
    stats_ticks: u64,
    pub first_frame: Option<Instant>,
    pub server_video_size: Option<(u32, u32)>,
    pub title_info: Option<String>,
}

pub struct App {
    pub rt: tokio::runtime::Runtime,
    http: reqwest::Client,
    pub paths: Paths,
    pub settings: Settings,
    pub shared: Arc<Shared>,
    pub main_tx: Sender<MainMsg>,
    backend_tx: Sender<Backend>,
    backend_rx: Receiver<Backend>,
    pub frame_notify: Sender<()>,
    pub account: Option<Arc<Account>>,
    pub gamertag: Option<String>,
    pub screen: Screen,
    pub tab: Tab,
    pub consoles: Load<Vec<Console>>,
    home_base: Option<String>,
    pub cloud: Load<CloudLibrary>,
    pub images: Images,
    pub search: String,
    pub show_all_games: bool,
    pub stream: Option<ActiveStream>,
    next_stream_id: u64,
    pub toast: Option<(String, Instant)>,
    pub gamepads: Vec<(u8, String)>,
    pub mock: Option<String>,
    pub show_stats: bool,
    pub quit: bool,
    pub renderer_info: String,
    pub request_focus_reset: bool,
    /// Frames since the focus reset was requested (0 = not started).
    pub focus_reset_frames: u32,
    /// Controller focus moves to apply during the next UI pass.
    pub pending_focus_moves: Vec<egui::FocusDirection>,
}

impl App {
    pub fn new(
        paths: Paths,
        settings: Settings,
        shared: Arc<Shared>,
        main_tx: Sender<MainMsg>,
        frame_notify: Sender<()>,
        wake: Sender<()>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("nimbus-net")
            .build()
            .expect("tokio runtime");
        let http = reqwest::Client::builder()
            .user_agent(concat!("Nimbus/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        let (backend_tx, backend_rx) = unbounded();
        let images = Images::new(
            rt.handle().clone(),
            http.clone(),
            paths.cache_dir.join("images"),
            wake,
        );
        let show_stats = settings.show_stats;
        let mut app = Self {
            rt,
            http,
            paths,
            settings,
            shared,
            main_tx,
            backend_tx,
            backend_rx,
            frame_notify,
            account: None,
            gamertag: None,
            screen: Screen::Startup,
            tab: Tab::Consoles,
            consoles: Load::NotLoaded,
            home_base: None,
            cloud: Load::NotLoaded,
            images,
            search: String::new(),
            show_all_games: false,
            stream: None,
            next_stream_id: 1,
            toast: None,
            gamepads: Vec::new(),
            mock: std::env::var("NIMBUS_MOCK").ok(),
            show_stats,
            quit: false,
            renderer_info: String::new(),
            request_focus_reset: true,
            focus_reset_frames: 0,
            pending_focus_moves: Vec::new(),
        };
        app.tab = match std::env::var("NIMBUS_TAB").as_deref() {
            Ok("cloud") => Tab::Cloud,
            Ok("settings") => Tab::Settings,
            _ => Tab::Consoles,
        };
        app.startup();
        app
    }

    fn spawn<F>(&self, f: F)
    where
        F: Future<Output = Backend> + Send + 'static,
    {
        let tx = self.backend_tx.clone();
        let waker = self.images.waker();
        self.rt.spawn(async move {
            let msg = f.await;
            let _ = tx.send(msg);
            let _ = waker.try_send(());
        });
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn startup(&mut self) {
        if self.mock.is_some() {
            tracing::warn!("MOCK MODE: using local mock server, no sign-in");
            self.gamertag = Some("MockPlayer".into());
            self.screen = Screen::Home;
            self.refresh_all();
            return;
        }
        let auth = Auth::new(self.http.clone());
        match Account::load(auth, &self.paths.account()) {
            Some(acc) => {
                self.account = Some(Arc::new(acc));
                self.screen = Screen::Home;
                self.refresh_all();
            }
            None => self.screen = Screen::Login(LoginState::Idle),
        }
    }

    // ----------------------------------------------------------------- auth

    pub fn begin_login(&mut self) {
        self.screen = Screen::Login(LoginState::Requesting);
        let auth = Auth::new(self.http.clone());
        let tx = self.backend_tx.clone();
        let waker = self.images.waker();
        let path = self.paths.account();
        self.rt.spawn(async move {
            let code = match auth.start_device_code().await {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Backend::DeviceCode(Err(e.to_string())));
                    let _ = waker.try_send(());
                    return;
                }
            };
            let _ = tx.send(Backend::DeviceCode(Ok(code.clone())));
            let _ = waker.try_send(());
            let result = async {
                let token = auth.device_code_login(&code).await?;
                let acc = Account::from_login(auth.clone(), Some(&path), token)?;
                acc.persist().await?;
                // Validate the whole chain once (and fetch the gamertag).
                acc.web_xsts().await?;
                Ok::<_, xcloud::Error>(Arc::new(acc))
            }
            .await;
            let _ = tx.send(Backend::LoggedIn(result.map_err(|e| e.to_string())));
            let _ = waker.try_send(());
        });
    }

    pub fn sign_out(&mut self) {
        if let Some(acc) = self.account.take() {
            acc.delete_saved();
        }
        self.gamertag = None;
        self.consoles = Load::NotLoaded;
        self.cloud = Load::NotLoaded;
        self.screen = Screen::Login(LoginState::Idle);
    }

    // ----------------------------------------------------------- libraries

    pub fn refresh_all(&mut self) {
        self.refresh_consoles();
        self.refresh_cloud();
        if let Some(acc) = self.account.clone() {
            self.spawn(async move {
                match acc.web_xsts().await {
                    Ok(t) => Backend::Profile(t.gamertag.unwrap_or_default()),
                    Err(xcloud::Error::ReauthRequired(e)) => Backend::AuthExpired(e),
                    Err(e) => Backend::Profile(format!("(offline: {e})")),
                }
            });
        }
    }

    pub fn refresh_consoles(&mut self) {
        if self.consoles.is_loading() {
            return;
        }
        self.consoles = Load::Loading;
        let http = self.http.clone();
        let settings = self.settings.clone();
        if let Some(base) = self.mock.clone() {
            self.spawn(async move {
                let api = GssvApi::new(
                    http,
                    &base,
                    "mock",
                    StreamKind::Home,
                    settings.resolution,
                    &settings.locale,
                );
                Backend::Consoles(
                    api.consoles()
                        .await
                        .map(|c| (base, c))
                        .map_err(|e| e.to_string()),
                )
            });
            return;
        }
        let Some(acc) = self.account.clone() else {
            return;
        };
        self.spawn(async move {
            let result = async {
                let token = acc.xhome_token().await?;
                let mut regions = token.offering_settings.regions.clone();
                regions.sort_by_key(|r| !r.is_default);
                let mut last_err = None;
                for region in &regions {
                    let api = GssvApi::new(
                        http.clone(),
                        &region.base_uri,
                        &token.gs_token,
                        StreamKind::Home,
                        settings.resolution,
                        &settings.locale,
                    );
                    match api.consoles().await {
                        Ok(c) if !c.is_empty() => return Ok((region.base_uri.clone(), c)),
                        Ok(_) => {}
                        Err(e) => last_err = Some(e),
                    }
                }
                match (last_err, regions.first()) {
                    (Some(e), _) => Err(e),
                    (None, Some(r)) => Ok((r.base_uri.clone(), Vec::new())),
                    (None, None) => Ok((String::new(), Vec::new())),
                }
            }
            .await;
            match result {
                Err(xcloud::Error::ReauthRequired(e)) => Backend::AuthExpired(e),
                r => Backend::Consoles(r.map_err(|e| e.to_string())),
            }
        });
    }

    pub fn refresh_cloud(&mut self) {
        if self.cloud.is_loading() {
            return;
        }
        self.cloud = Load::Loading;
        let http = self.http.clone();
        let settings = self.settings.clone();
        if let Some(base) = self.mock.clone() {
            self.spawn(async move {
                let api = GssvApi::new(
                    http,
                    &base,
                    "mock",
                    StreamKind::Cloud,
                    settings.resolution,
                    &settings.locale,
                );
                let r = api.titles().await.map(|titles| {
                    Some(CloudLibrary {
                        offering: Offering::XCloud,
                        region: "Mock".into(),
                        regions: vec!["Mock".into()],
                        base_uri: base.clone(),
                        games: titles
                            .into_iter()
                            .map(|t| CloudGame {
                                name: t.details.product_id.clone().unwrap_or(t.title_id.clone()),
                                title_id: t.title_id,
                                publisher: "Mock".into(),
                                image: None,
                                playable: true,
                            })
                            .collect(),
                        recent: Vec::new(),
                    })
                });
                Backend::Cloud(r.map_err(|e| e.to_string()))
            });
            return;
        }
        let Some(acc) = self.account.clone() else {
            return;
        };
        self.spawn(async move {
            let result = async {
                let Some((offering, token)) = acc.xcloud_token().await? else {
                    return Ok(None);
                };
                let region = settings
                    .xcloud_region
                    .as_ref()
                    .and_then(|name| {
                        token
                            .offering_settings
                            .regions
                            .iter()
                            .find(|r| &r.name == name)
                    })
                    .or_else(|| token.default_region())
                    .cloned()
                    .ok_or_else(|| xcloud::Error::Session("no xCloud region".into()))?;
                let api = GssvApi::new(
                    http.clone(),
                    &region.base_uri,
                    &token.gs_token,
                    StreamKind::Cloud,
                    settings.resolution,
                    &settings.locale,
                );
                let titles = api.titles().await?;
                let recent = api
                    .recent_titles()
                    .await
                    .map(|t| t.into_iter().map(|t| t.title_id).collect())
                    .unwrap_or_default();
                let product_ids: Vec<String> = titles
                    .iter()
                    .filter_map(|t| t.details.product_id.clone())
                    .collect();
                let products =
                    xcloud::catalog::products(&http, &token.market, &settings.locale, &product_ids)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!("catalog lookup failed: {e}");
                            Default::default()
                        });
                let mut games: Vec<CloudGame> = titles
                    .into_iter()
                    .map(|t| {
                        let product = t
                            .details
                            .product_id
                            .as_ref()
                            .and_then(|id| products.get(id));
                        CloudGame {
                            name: product
                                .map(|p| p.product_title.clone())
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| t.title_id.clone()),
                            publisher: product
                                .and_then(|p| p.publisher_name.clone())
                                .unwrap_or_default(),
                            image: product.and_then(|p| p.tile_url()),
                            playable: t.playable(),
                            title_id: t.title_id,
                        }
                    })
                    .collect();
                games.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                let regions = token
                    .offering_settings
                    .regions
                    .iter()
                    .map(|r| r.name.clone())
                    .collect();
                Ok(Some(CloudLibrary {
                    offering,
                    regions,
                    region: region.name,
                    base_uri: region.base_uri,
                    games,
                    recent,
                }))
            }
            .await;
            match result {
                Err(xcloud::Error::ReauthRequired(e)) => Backend::AuthExpired(e),
                r => Backend::Cloud(r.map_err(|e: xcloud::Error| e.to_string())),
            }
        });
    }

    // ------------------------------------------------------------- streams

    pub fn start_stream(&mut self, target: StreamTarget) {
        if self.stream.is_some() {
            return;
        }
        let id = self.next_stream_id;
        self.next_stream_id += 1;
        let (video_tx, video_rx) = unbounded();
        let (audio_tx, audio_rx) = unbounded();
        let (events_tx, events_rx) = unbounded();
        let video = VideoPipeline::spawn(
            video_rx,
            self.settings.decoder,
            self.shared.clone(),
            self.frame_notify.clone(),
        );
        let audio = AudioPipeline::spawn(audio_rx, self.settings.audio_buffer_ms);
        let sinks = StreamSinks {
            video: video_tx,
            audio: audio_tx,
            events: events_tx,
        };
        let cfg = StreamConfig {
            resolution: self.settings.resolution,
            codec: self.settings.codec,
            max_video_kbps: (self.settings.max_bitrate_mbps > 0)
                .then_some(self.settings.max_bitrate_mbps * 1000),
            locale: self.settings.locale.clone(),
        };
        let created: Arc<Mutex<Option<(GssvApi, SessionInfo)>>> = Arc::default();

        let http = self.http.clone();
        let account = self.account.clone();
        let mock = self.mock.clone();
        let home_base = self.home_base.clone();
        let settings = self.settings.clone();
        let cloud_base = match &self.cloud {
            Load::Loaded(c) => Some(c.base_uri.clone()),
            _ => None,
        };
        let tx = self.backend_tx.clone();
        let waker = self.images.waker();
        let created2 = created.clone();
        let target2 = target.clone();
        let task = self.rt.spawn(async move {
            let result = async {
                let (kind, target_id) = match &target2 {
                    StreamTarget::Home { server_id, .. } => (StreamKind::Home, server_id.clone()),
                    StreamTarget::Cloud { title_id, .. } => (StreamKind::Cloud, title_id.clone()),
                };
                let api = if let Some(base) = mock {
                    GssvApi::new(
                        http,
                        &base,
                        "mock",
                        kind,
                        settings.resolution,
                        &settings.locale,
                    )
                } else {
                    let acc = account.clone().ok_or(xcloud::Error::NotSignedIn)?;
                    match kind {
                        StreamKind::Home => {
                            let t = acc.xhome_token().await?;
                            let base = home_base
                                .or_else(|| t.default_region().map(|r| r.base_uri.clone()))
                                .ok_or_else(|| {
                                    xcloud::Error::Session("no remote play region".into())
                                })?;
                            GssvApi::new(
                                http,
                                &base,
                                &t.gs_token,
                                kind,
                                settings.resolution,
                                &settings.locale,
                            )
                        }
                        StreamKind::Cloud => {
                            let (_, t) = acc.xcloud_token().await?.ok_or_else(|| {
                                xcloud::Error::Session(
                                    "Cloud gaming is not available for this account".into(),
                                )
                            })?;
                            let base = cloud_base
                                .or_else(|| t.default_region().map(|r| r.base_uri.clone()))
                                .ok_or_else(|| xcloud::Error::Session("no xCloud region".into()))?;
                            GssvApi::new(
                                http,
                                &base,
                                &t.gs_token,
                                kind,
                                settings.resolution,
                                &settings.locale,
                            )
                        }
                    }
                };
                let api2 = api.clone();
                let tx2 = tx.clone();
                let waker2 = waker.clone();
                xcloud::stream::start_stream(account, api, target_id, cfg, sinks, move |p| {
                    if let SessionProgress::Created(info) = &p {
                        *created2.lock() = Some((api2.clone(), info.clone()));
                    }
                    let _ = tx2.send(Backend::StreamProgress(id, p));
                    let _ = waker2.try_send(());
                })
                .await
            }
            .await;
            let _ = tx.send(Backend::StreamStarted(
                id,
                result.map_err(|e| e.to_string()),
            ));
            let _ = waker.try_send(());
        });

        self.stream = Some(ActiveStream {
            id,
            name: target.name().to_owned(),
            session: None,
            task: Some(task),
            created,
            video: Some(video),
            audio: Some(audio),
            events: events_rx,
            net: NetworkStats::default(),
            decode: DecodeStats::default(),
            ready: false,
            menu_open: false,
            dialog: None,
            present_times: VecDeque::new(),
            fps: 0.0,
            latency_ms: 0.0,
            queue_ms: 0.0,
            render_ms: 0.0,
            stats_ticks: 0,
            first_frame: None,
            server_video_size: None,
            title_info: None,
        });
        self.screen = Screen::Connecting(Connecting {
            name: target.name().to_owned(),
            status: "Requesting a session…".into(),
            started: Instant::now(),
            wait_seconds: None,
        });
    }

    /// Stop the current stream (user action or disconnect).
    pub fn end_stream(&mut self, reason: Option<String>) {
        let Some(mut s) = self.stream.take() else {
            return;
        };
        *self.shared.stream.lock() = None;
        self.shared
            .menu_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(task) = s.task.take() {
            task.abort();
        }
        let session = s.session.take();
        let created = s.created.lock().take();
        self.rt.spawn(async move {
            if let Some(session) = session {
                session.stop().await;
            } else if let Some((api, info)) = created {
                let _ = api.stop(&info).await;
            }
        });
        // Join pipeline threads off the UI thread.
        let video = s.video.take();
        let audio = s.audio.take();
        std::thread::spawn(move || {
            if let Some(v) = video {
                v.join();
            }
            if let Some(a) = audio {
                a.join();
            }
        });
        let _ = self.main_tx.send(MainMsg::StopRumble);
        let _ = self.main_tx.send(MainMsg::ShowCursor(true));
        if self.settings.fullscreen_on_stream {
            let _ = self.main_tx.send(MainMsg::SetFullscreen(false));
        }
        self.screen = Screen::Home;
        self.request_focus_reset = true;
        if let Some(r) = reason {
            self.toast(r);
        }
        // Console power state / recent titles likely changed.
        self.consoles = Load::NotLoaded;
        self.refresh_consoles();
    }

    /// Synchronously stop any stream (app exit).
    pub fn shutdown(&mut self) {
        let Some(mut s) = self.stream.take() else {
            return;
        };
        *self.shared.stream.lock() = None;
        if let Some(task) = s.task.take() {
            task.abort();
        }
        let session = s.session.take();
        let created = s.created.lock().take();
        self.rt.block_on(async move {
            let stop = async {
                if let Some(session) = session {
                    session.stop().await;
                } else if let Some((api, info)) = created {
                    let _ = api.stop(&info).await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(3), stop).await;
        });
    }

    pub fn is_streaming(&self) -> bool {
        matches!(self.screen, Screen::Streaming)
    }

    pub fn take_picture(&mut self) -> Option<DecodedPicture> {
        let s = self.stream.as_ref()?;
        s.video.as_ref()?.mailbox.take()
    }

    /// Called after a picture has been presented.
    pub fn on_presented(&mut self, pic: &DecodedPicture, presented: Instant) {
        let Some(s) = self.stream.as_mut() else {
            return;
        };
        if s.first_frame.is_none() {
            s.first_frame = Some(presented);
            let _ = self.main_tx.send(MainMsg::ShowCursor(false));
        }
        s.present_times.push_back(presented);
        while s
            .present_times
            .front()
            .is_some_and(|t| presented.duration_since(*t) > Duration::from_secs(1))
        {
            s.present_times.pop_front();
        }
        s.fps = s.present_times.len() as f32;
        let ema = |prev: f32, v: f32| if prev == 0.0 { v } else { prev * 0.9 + v * 0.1 };
        let ms = |a: Instant, b: Instant| b.saturating_duration_since(a).as_secs_f32() * 1000.0;
        s.latency_ms = ema(s.latency_ms, ms(pic.arrival, presented));
        s.queue_ms = ema(s.queue_ms, ms(pic.arrival, pic.decode_start));
        s.render_ms = ema(s.render_ms, ms(pic.decoded, presented));
        if let Some(sess) = &s.session {
            let h = &sess.handle;
            h.send(Command::FrameMetadata(FrameMetadata {
                server_data_key: pic.rtp_timestamp,
                first_packet_arrival_ms: h.ms(pic.arrival),
                submitted_ms: h.ms(pic.decode_start),
                decoded_ms: h.ms(pic.decoded),
                rendered_ms: h.ms(presented),
            }));
        }
    }

    pub fn toggle_menu(&mut self) {
        if let Some(s) = self.stream.as_mut() {
            s.menu_open = !s.menu_open;
            self.shared
                .menu_open
                .store(s.menu_open, std::sync::atomic::Ordering::Relaxed);
            let _ = self.main_tx.send(MainMsg::ShowCursor(s.menu_open));
            self.request_focus_reset = true;
        }
    }

    pub fn answer_dialog(&mut self, index: usize) {
        if let Some(s) = self.stream.as_mut()
            && let Some(d) = s.dialog.take()
        {
            if let Some(sess) = &s.session {
                sess.handle.send(Command::DialogResult {
                    transaction_id: d.transaction_id,
                    result: index as u32,
                });
            }
            self.shared
                .menu_open
                .store(s.menu_open, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn save_settings(&mut self) {
        self.settings.save(&self.paths.settings());
        self.shared.set_deadzone(self.settings.deadzone);
        self.shared.vibration.store(
            self.settings.vibration,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    // ------------------------------------------------------------ updates

    /// Handle a controller navigation action at the app level. Returns true
    /// if consumed (otherwise it is forwarded to egui focus navigation).
    pub fn on_nav(&mut self, nav: NavAction) -> bool {
        match (&self.screen, nav) {
            (Screen::Home, NavAction::PrevTab | NavAction::NextTab) => {
                let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0) as i32;
                let d = if nav == NavAction::NextTab { 1 } else { -1 };
                self.tab = Tab::ALL[((i + d).rem_euclid(Tab::ALL.len() as i32)) as usize];
                self.request_focus_reset = true;
                true
            }
            (Screen::Home, NavAction::Secondary) => {
                self.refresh_all();
                true
            }
            (Screen::Connecting(_), NavAction::Back) => {
                self.end_stream(Some("Cancelled".into()));
                true
            }
            (Screen::Streaming, NavAction::Back) => {
                if self.stream.as_ref().is_some_and(|s| s.dialog.is_none()) {
                    self.toggle_menu();
                }
                true
            }
            _ => false,
        }
    }

    pub fn update(&mut self, ctx: &egui::Context) {
        self.images.poll(ctx);
        while let Ok(msg) = self.backend_rx.try_recv() {
            self.on_backend(ctx, msg);
        }
        self.poll_stream();
        if let Some((_, t)) = &self.toast
            && t.elapsed() > Duration::from_secs(6)
        {
            self.toast = None;
        }
    }

    fn on_backend(&mut self, ctx: &egui::Context, msg: Backend) {
        match msg {
            Backend::DeviceCode(Ok(code)) => {
                let qr = crate::ui::images::qr_texture(ctx, &code.verification_uri_complete());
                self.screen = Screen::Login(LoginState::Code { code, qr });
            }
            Backend::DeviceCode(Err(e)) => self.screen = Screen::Login(LoginState::Error(e)),
            Backend::LoggedIn(Ok(acc)) => {
                self.account = Some(acc);
                self.screen = Screen::Home;
                self.request_focus_reset = true;
                self.refresh_all();
            }
            Backend::LoggedIn(Err(e)) => self.screen = Screen::Login(LoginState::Error(e)),
            Backend::Profile(tag) => self.gamertag = Some(tag),
            Backend::Consoles(Ok((base, list))) => {
                if !base.is_empty() {
                    self.home_base = Some(base);
                }
                // Debug automation: NIMBUS_AUTOSTREAM=home starts the first console.
                if std::env::var("NIMBUS_AUTOSTREAM").as_deref() == Ok("home")
                    && self.stream.is_none()
                    && self.next_stream_id == 1
                    && let Some(c) = list.first()
                {
                    let target = StreamTarget::Home {
                        server_id: c.server_id.clone(),
                        name: c.device_name.clone(),
                    };
                    self.consoles = Load::Loaded(list);
                    self.start_stream(target);
                    return;
                }
                self.consoles = Load::Loaded(list);
            }
            Backend::Consoles(Err(e)) => self.consoles = Load::Failed(e),
            Backend::Cloud(Ok(Some(lib))) => {
                for g in &lib.games {
                    if let Some(url) = &g.image {
                        self.images.prefetch(url);
                    }
                }
                self.cloud = Load::Loaded(lib);
            }
            Backend::Cloud(Ok(None)) => {
                self.cloud = Load::Failed(
                    "Xbox Cloud Gaming isn't available for this account or region.".into(),
                )
            }
            Backend::Cloud(Err(e)) => self.cloud = Load::Failed(e),
            Backend::AuthExpired(e) => {
                tracing::warn!("auth expired: {e}");
                self.sign_out();
                self.toast("Your session expired, please sign in again.");
            }
            Backend::StreamProgress(id, p) => {
                if self.stream.as_ref().map(|s| s.id) != Some(id) {
                    return;
                }
                if let Screen::Connecting(c) = &mut self.screen {
                    c.status = match p {
                        SessionProgress::Requesting => "Requesting a session…".into(),
                        SessionProgress::Created(_) => "Session created…".into(),
                        SessionProgress::Queued { wait_seconds } => {
                            c.wait_seconds = wait_seconds;
                            "Waiting in queue…".into()
                        }
                        SessionProgress::Provisioning => "Starting the game…".into(),
                        SessionProgress::Authorizing => "Authorizing…".into(),
                        SessionProgress::Negotiating => "Negotiating stream…".into(),
                        SessionProgress::Connecting => "Connecting…".into(),
                    };
                }
            }
            Backend::StreamStarted(id, result) => {
                if self.stream.as_ref().map(|s| s.id) != Some(id) {
                    if let Ok(session) = result {
                        self.rt.spawn(session.stop());
                    }
                    return;
                }
                match result {
                    Ok(session) => {
                        if let Some(s) = self.stream.as_mut() {
                            s.task = None;
                            *self.shared.stream.lock() = Some(session.handle.clone());
                            // Announce extra controllers already plugged in.
                            for (slot, _) in self.gamepads.iter().filter(|(s, _)| *s != 0) {
                                session.handle.send(Command::GamepadConnected(*slot, true));
                            }
                            s.session = Some(session);
                        }
                        self.screen = Screen::Streaming;
                        if std::env::var("NIMBUS_DEBUG_MENU").is_ok() {
                            self.toggle_menu();
                        }
                        if self.settings.fullscreen_on_stream {
                            let _ = self.main_tx.send(MainMsg::SetFullscreen(true));
                        }
                    }
                    Err(e) => self.end_stream(Some(format!("Could not start the stream: {e}"))),
                }
            }
        }
    }

    fn poll_stream(&mut self) {
        let mut disconnect = None;
        let Some(s) = self.stream.as_mut() else {
            return;
        };
        if let Some(v) = &s.video {
            s.decode = v.stats.lock().clone();
        }
        while let Ok(ev) = s.events.try_recv() {
            match ev {
                StreamEvent::Connected => tracing::info!("transport connected"),
                StreamEvent::Ready => s.ready = true,
                StreamEvent::Disconnected(reason) => disconnect = Some(reason),
                StreamEvent::VideoSize { width, height } => {
                    s.server_video_size = Some((width, height))
                }
                StreamEvent::Rumble(v) => {
                    tracing::debug!(?v, "rumble");
                    if self.settings.vibration {
                        let _ = self.main_tx.send(MainMsg::Rumble(v));
                    }
                }
                StreamEvent::Dialog(d) => {
                    s.dialog = Some(d);
                    self.shared
                        .menu_open
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.request_focus_reset = true;
                }
                StreamEvent::TitleInfo(v) => {
                    s.title_info = v
                        .get("titleName")
                        .or_else(|| v.get("TitleName"))
                        .and_then(|t| t.as_str())
                        .map(str::to_owned);
                }
                StreamEvent::Stats(n) => {
                    s.net = n;
                    s.stats_ticks += 1;
                    if s.stats_ticks % 5 == 0 {
                        tracing::info!(
                            fps = s.fps,
                            latency_ms = format!("{:.1}", s.latency_ms),
                            net_ms = format!("{:.1}", s.queue_ms),
                            decode_ms = format!("{:.1}", s.decode.decode_ms_avg),
                            render_ms = format!("{:.1}", s.render_ms),
                            mbps = format!("{:.1}", s.net.video_kbps / 1000.0),
                            decoded = s.decode.decoded_frames,
                            skipped = s.decode.skipped_frames,
                            "stream stats"
                        );
                    }
                }
            }
        }
        if let Some(reason) = disconnect {
            self.end_stream(Some(reason));
        }
    }
}
