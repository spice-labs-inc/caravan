//! Self-screenshot mode for reviewing the UI without a desktop capture tool:
//! `CARAVAN_SCREENSHOT_DIR=<dir>` walks the views (`CARAVAN_SCREENSHOT_VIEWS`,
//! comma separated, default all top-level views) after
//! `CARAVAN_SCREENSHOT_DELAY_SECS` (default 20) so the pollers have data,
//! writes `<view>.png` per view, then closes the window.

use crate::ui::sidebar::{Sidebar, View};
use eframe::egui;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct ShotPlan {
    dir: PathBuf,
    views: Vec<View>,
    idx: usize,
    next_at: Instant,
    /// Frames rendered since the current view was selected; the capture is
    /// requested once the layout has settled.
    frames_since_nav: Option<u32>,
    requested: bool,
}

const SETTLE_FRAMES: u32 = 6;

impl ShotPlan {
    pub fn from_env() -> Option<Self> {
        let dir = PathBuf::from(std::env::var("CARAVAN_SCREENSHOT_DIR").ok()?);
        std::fs::create_dir_all(&dir).ok()?;
        let delay = std::env::var("CARAVAN_SCREENSHOT_DELAY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20u64);
        let views: Vec<View> = std::env::var("CARAVAN_SCREENSHOT_VIEWS")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter_map(|v| View::from_str(v.trim()))
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![
                    View::Apps,
                    View::Vault,
                    View::Infra,
                    View::Libraries,
                    View::PullRequests,
                    View::Issues,
                    View::Settings,
                ]
            });
        if views.is_empty() {
            return None;
        }
        tracing::info!(dir = %dir.display(), views = views.len(), delay, "Screenshot mode");
        Some(Self {
            dir,
            views,
            idx: 0,
            next_at: Instant::now() + Duration::from_secs(delay),
            frames_since_nav: None,
            requested: false,
        })
    }

    /// Drive one frame: select the next view when due, request the capture
    /// once it has settled, save the image when it arrives, close at the end.
    pub fn step(&mut self, ctx: &egui::Context, sidebar: &mut Sidebar) {
        ctx.request_repaint_after(Duration::from_millis(50));
        if self.idx >= self.views.len() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if let Some(image) = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        }) {
            let view = &self.views[self.idx];
            let name = view.as_str().replace(':', "_");
            let path = self.dir.join(format!("{}.png", name));
            match save_png(&image, &path) {
                Ok(()) => tracing::info!(path = %path.display(), "Screenshot saved"),
                Err(e) => tracing::warn!(err = %e, "Screenshot save failed"),
            }
            self.idx += 1;
            self.frames_since_nav = None;
            self.requested = false;
            self.next_at = Instant::now() + Duration::from_millis(400);
            return;
        }
        if Instant::now() < self.next_at {
            return;
        }
        match self.frames_since_nav {
            None => {
                sidebar.navigate(self.views[self.idx].clone());
                self.frames_since_nav = Some(0);
            }
            Some(n) if n < SETTLE_FRAMES => self.frames_since_nav = Some(n + 1),
            Some(_) if !self.requested => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.requested = true;
            }
            Some(_) => {}
        }
    }
}

fn save_png(image: &egui::ColorImage, path: &std::path::Path) -> anyhow::Result<()> {
    let [w, h] = image.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for px in &image.pixels {
        bytes.extend_from_slice(&px.to_array());
    }
    let img = image::RgbaImage::from_raw(w as u32, h as u32, bytes)
        .ok_or_else(|| anyhow::anyhow!("image buffer size mismatch"))?;
    img.save(path)?;
    Ok(())
}
