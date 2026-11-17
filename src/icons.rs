//! Background icon loading with a bounded texture cache.
//!
//! The UI asks for icons of visible cards only. A worker thread reads them from
//! the game's VPK (or a preview file as fallback), decodes off the UI thread and
//! hands back pixels. Newest requests are served first, so fast scrolling does
//! not leave the screen waiting for cards that are already gone.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui;

use crate::vpk::{IconImage, IconPak};

/// Max textures kept on the GPU; ~170 KB each at 256x170.
const MAX_TEXTURES: usize = 600;
/// Pending requests beyond this are dropped (oldest first).
const MAX_QUEUE: usize = 256;

struct Job {
    key: String,
    icon: Option<String>,
    preview: Option<PathBuf>,
}

struct Done {
    key: String,
    image: Option<egui::ColorImage>,
}

struct Queue {
    jobs: Mutex<(Vec<Job>, bool)>, // (stack, shutdown)
    signal: Condvar,
}

pub struct IconLoader {
    queue: Arc<Queue>,
    rx: Receiver<Done>,
    textures: HashMap<String, (egui::TextureHandle, u64)>,
    pending: HashSet<String>,
    missing: HashSet<String>,
    frame: u64,
    pub source: String,
}

impl IconLoader {
    pub fn new(ctx: &egui::Context, game_dir: Option<PathBuf>) -> Self {
        let queue = Arc::new(Queue {
            jobs: Mutex::new((Vec::new(), false)),
            signal: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel();

        // Open the pak synchronously so the UI can report where icons come from.
        let pak = game_dir.as_deref().map(IconPak::open);
        let source = match &pak {
            Some(Ok(p)) => crate::trf!("иконки из игры: {}", "icons from the game: {}", p.len()),
            Some(Err(e)) => crate::trf!("иконки из игры недоступны ({e})", "icons from the game unavailable ({e})"),
            None => crate::tr!("игра не найдена — превью из папки assets", "game not found, previews from the assets folder").to_string(),
        };
        let pak = pak.and_then(Result::ok);

        let worker_queue = Arc::clone(&queue);
        let worker_ctx = ctx.clone();
        std::thread::Builder::new()
            .name("icon-loader".into())
            .spawn(move || worker(worker_queue, tx, worker_ctx, pak))
            .ok();

        Self {
            queue,
            rx,
            textures: HashMap::new(),
            pending: HashSet::new(),
            missing: HashSet::new(),
            frame: 0,
            source,
        }
    }

    /// Call once per frame before drawing: uploads finished icons, evicts old ones.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        while let Ok(done) = self.rx.try_recv() {
            self.pending.remove(&done.key);
            match done.image {
                Some(img) => {
                    let tex = ctx.load_texture(&done.key, img, egui::TextureOptions::LINEAR);
                    self.textures.insert(done.key, (tex, self.frame));
                }
                None => {
                    self.missing.insert(done.key);
                }
            }
        }

        if self.textures.len() > MAX_TEXTURES {
            let mut ages: Vec<(u64, String)> = self
                .textures
                .iter()
                .map(|(k, (_, used))| (*used, k.clone()))
                .collect();
            ages.sort_unstable();
            let drop = self.textures.len() - MAX_TEXTURES * 3 / 4;
            for (_, key) in ages.into_iter().take(drop) {
                self.textures.remove(&key);
            }
        }
    }

    /// Texture for a card, or `None` while it loads / when there is no image.
    pub fn get(
        &mut self,
        key: &str,
        icon: Option<&str>,
        preview: Option<PathBuf>,
    ) -> Option<egui::TextureHandle> {
        if let Some((tex, used)) = self.textures.get_mut(key) {
            *used = self.frame;
            return Some(tex.clone());
        }
        if self.pending.contains(key) || self.missing.contains(key) {
            return None;
        }
        if icon.is_none() && preview.is_none() {
            self.missing.insert(key.to_string());
            return None;
        }

        self.pending.insert(key.to_string());
        let mut guard = self.queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
        guard.0.push(Job {
            key: key.to_string(),
            icon: icon.map(str::to_string),
            preview,
        });
        if guard.0.len() > MAX_QUEUE {
            let overflow = guard.0.len() - MAX_QUEUE;
            for job in guard.0.drain(..overflow) {
                self.pending.remove(&job.key);
            }
        }
        drop(guard);
        self.queue.signal.notify_one();
        None
    }
}

impl Drop for IconLoader {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.queue.jobs.lock() {
            guard.1 = true;
        }
        self.queue.signal.notify_all();
    }
}

fn worker(queue: Arc<Queue>, tx: Sender<Done>, ctx: egui::Context, mut pak: Option<IconPak>) {
    loop {
        let job = {
            let mut guard = queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
            loop {
                if guard.1 {
                    return;
                }
                if let Some(job) = guard.0.pop() {
                    break job;
                }
                guard = queue
                    .signal
                    .wait(guard)
                    .unwrap_or_else(|p| p.into_inner());
            }
        };

        let from_pak = match (&mut pak, job.icon.as_deref()) {
            (Some(pak), Some(icon)) => match pak.icon_image(icon) {
                Some(IconImage::Encoded(bytes)) => decode(&bytes),
                Some(IconImage::Rgba { width, height, pixels }) => Some(
                    egui::ColorImage::from_rgba_unmultiplied([width, height], &pixels),
                ),
                None => None,
            },
            _ => None,
        };
        let image = from_pak.or_else(|| {
            job.preview
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|bytes| decode(&bytes))
        });

        if tx.send(Done { key: job.key, image }).is_err() {
            return; // UI side is gone
        }
        ctx.request_repaint();
    }
}

fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}
