//! Artwork loading: HTTP → disk cache → decode/resize off-thread → egui
//! texture.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};

enum Slot {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

pub struct Images {
    rt: tokio::runtime::Handle,
    http: reqwest::Client,
    dir: PathBuf,
    slots: HashMap<String, Slot>,
    tx: Sender<(String, Option<egui::ColorImage>)>,
    rx: Receiver<(String, Option<egui::ColorImage>)>,
    wake: Sender<()>,
    limit: Arc<tokio::sync::Semaphore>,
}

const MAX_SIDE: u32 = 360;

fn cache_name(url: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
        img.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

impl Images {
    pub fn new(
        rt: tokio::runtime::Handle,
        http: reqwest::Client,
        dir: PathBuf,
        wake: Sender<()>,
    ) -> Self {
        let (tx, rx) = unbounded();
        Self {
            rt,
            http,
            dir,
            slots: HashMap::new(),
            tx,
            rx,
            wake,
            limit: Arc::new(tokio::sync::Semaphore::new(6)),
        }
    }

    pub fn waker(&self) -> Sender<()> {
        self.wake.clone()
    }

    pub fn prefetch(&mut self, url: &str) {
        if self.slots.contains_key(url) {
            return;
        }
        self.slots.insert(url.to_owned(), Slot::Loading);
        let url = url.to_owned();
        let http = self.http.clone();
        let tx = self.tx.clone();
        let wake = self.wake.clone();
        let path = self.dir.join(cache_name(&url));
        let limit = self.limit.clone();
        self.rt.spawn(async move {
            let _permit = limit.acquire().await;
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => Some(b),
                Err(_) => {
                    let fetched = async {
                        let r = http.get(&url).send().await.ok()?.error_for_status().ok()?;
                        r.bytes().await.ok().map(|b| b.to_vec())
                    }
                    .await;
                    if let Some(b) = &fetched {
                        if let Some(dir) = path.parent() {
                            let _ = tokio::fs::create_dir_all(dir).await;
                        }
                        let _ = tokio::fs::write(&path, b).await;
                    }
                    fetched
                }
            };
            let image = match bytes {
                Some(b) => tokio::task::spawn_blocking(move || decode(&b))
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let _ = tx.send((url, image));
            let _ = wake.try_send(());
        });
    }

    /// Texture for `url`, requesting it if needed.
    pub fn get(&mut self, url: &str) -> Option<egui::TextureId> {
        match self.slots.get(url) {
            Some(Slot::Ready(t)) => Some(t.id()),
            Some(_) => None,
            None => {
                self.prefetch(url);
                None
            }
        }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((url, img)) = self.rx.try_recv() {
            let slot = match img {
                Some(img) => Slot::Ready(ctx.load_texture(&url, img, egui::TextureOptions::LINEAR)),
                None => Slot::Failed,
            };
            self.slots.insert(url, slot);
        }
    }
}

/// Render a QR code into a texture.
pub fn qr_texture(ctx: &egui::Context, text: &str) -> Option<egui::TextureHandle> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let width = code.width();
    let quiet = 2;
    let scale = 6;
    let side = (width + quiet * 2) * scale;
    let colors = code.to_colors();
    let mut pixels = vec![egui::Color32::WHITE; side * side];
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = (x + quiet) * scale + dx;
                        let py = (y + quiet) * scale + dy;
                        pixels[py * side + px] = egui::Color32::BLACK;
                    }
                }
            }
        }
    }
    let img = egui::ColorImage::new([side, side], pixels);
    Some(ctx.load_texture("login-qr", img, egui::TextureOptions::NEAREST))
}
