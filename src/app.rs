use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{
    self, pos2, vec2, Align2, Color32, ColorImage, CursorIcon, FontId, Painter, Pos2, Rect,
    Sense, Stroke, StrokeKind, TextureHandle, TextureOptions,
};

use crate::config::{Config, Layout};
use crate::workers::{self, MqttStatus, SharedRef};

const STALE_AFTER: Duration = Duration::from_secs(10);
const ALERT_TIMEOUT: Duration = Duration::from_secs(600);
const ALERT_RED: Color32 = Color32::from_rgb(230, 40, 40);

struct Tile {
    name: String,
    want_height: Arc<AtomicU32>,
    texture: Option<TextureHandle>,
    aspect: f32,
}

struct TileStatus {
    stale: bool,
    error: Option<String>,
    alert: Option<Instant>,
}

struct Snapshot {
    caption: String,
    at: Option<DateTime<Local>>,
    texture: TextureHandle,
    aspect: f32,
}

pub struct App {
    shared: SharedRef,
    layout: Layout,
    clock_format: String,
    date_format: String,
    tiles: Vec<Tile>,
    snapshot: Option<Snapshot>,
    manual_focus: Option<usize>,
    auto_dismissed: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, cfg: Config) -> Self {
        let shared = SharedRef::default();
        let layout = cfg.layout;
        let clock_format = cfg.clock_format.clone();
        let date_format = cfg.date_format.clone();
        let (s, ctx, cfg) = (shared.clone(), cc.egui_ctx.clone(), Arc::new(cfg));
        thread::spawn(move || workers::start(cfg, s, ctx));
        Self {
            shared,
            layout,
            clock_format,
            date_format,
            tiles: Vec::new(),
            snapshot: None,
            manual_focus: None,
            auto_dismissed: false,
        }
    }

    fn sync(&mut self, ctx: &egui::Context) -> (Vec<TileStatus>, MqttStatus, Option<String>, bool) {
        let now = Instant::now();
        let mut s = self.shared.lock().unwrap();
        let ready = s.cameras.is_some();
        if self.tiles.is_empty() {
            if let Some(cams) = &s.cameras {
                self.tiles = cams
                    .iter()
                    .map(|(name, want)| Tile {
                        name: name.clone(),
                        want_height: want.clone(),
                        texture: None,
                        aspect: 0.0,
                    })
                    .collect();
            }
        }
        let mut status = Vec::with_capacity(self.tiles.len());
        for tile in &mut self.tiles {
            let cam = s.cams.entry(tile.name.clone()).or_default();
            cam.alerts.retain(|_, t| now.duration_since(*t) < ALERT_TIMEOUT);
            if let Some(img) = cam.frame.take() {
                tile.aspect = img.size[0] as f32 / img.size[1] as f32;
                upload(ctx, &mut tile.texture, &tile.name, img);
            }
            status.push(TileStatus {
                stale: cam.updated.is_none_or(|u| now.duration_since(u) > STALE_AFTER),
                error: cam.error.clone(),
                alert: cam.alerts.values().max().copied(),
            });
        }
        if let Some(shot) = s.snapshot.as_mut() {
            if let Some(img) = shot.image.take() {
                let aspect = img.size[0] as f32 / img.size[1] as f32;
                let mut texture = self.snapshot.take().map(|p| p.texture);
                upload(ctx, &mut texture, "snapshot", img);
                self.snapshot = Some(Snapshot {
                    caption: format!("{}  {}", shot.label, shot.camera),
                    at: shot.at,
                    texture: texture.unwrap(),
                    aspect,
                });
            }
        }
        (status, s.mqtt, s.startup_error.clone(), ready)
    }

    fn draw_info(&self, painter: &Painter, info: Rect, label_size: f32) {
        let clock_size = info.height() / 4.0;
        let local = Local::now();
        let top = info.min.y + info.height() * 0.05;
        painter.text(
            pos2(info.center().x, top),
            Align2::CENTER_TOP,
            local.format(&self.clock_format).to_string(),
            FontId::proportional(clock_size),
            Color32::from_gray(225),
        );
        painter.text(
            pos2(info.center().x, top + clock_size * 1.1),
            Align2::CENTER_TOP,
            local.format(&self.date_format).to_string(),
            FontId::proportional(label_size),
            Color32::from_gray(150),
        );
        if let Some(shot) = &self.snapshot {
            let y = top + clock_size * 1.1 + label_size * 1.6;
            let area = Rect::from_min_max(pos2(info.min.x + 8.0, y), info.max - vec2(8.0, 8.0));
            let area = fit(area, shot.aspect);
            draw_image(painter, &shot.texture, area);
            let when = shot
                .at
                .map(|t| t.format(&self.clock_format).to_string())
                .unwrap_or_else(|| "earlier".into());
            label(
                painter,
                area.min + vec2(4.0, 4.0),
                format!("{}  {}", shot.caption, when),
                label_size * 0.8,
                Color32::WHITE,
            );
        }
    }
}

fn upload(ctx: &egui::Context, slot: &mut Option<TextureHandle>, name: &str, img: ColorImage) {
    match slot {
        Some(t) => t.set(img, TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, img, TextureOptions::LINEAR)),
    }
}

fn fit(rect: Rect, aspect: f32) -> Rect {
    if aspect <= 0.0 {
        return rect;
    }
    let w = rect.width().min(rect.height() * aspect);
    Rect::from_center_size(rect.center(), vec2(w, w / aspect))
}

fn grid(rect: Rect, count: usize) -> Vec<Rect> {
    let cols = if count > 1 { 2 } else { 1 };
    let rows = count.div_ceil(cols).max(1);
    let size = vec2(rect.width() / cols as f32, rect.height() / rows as f32);
    (0..count)
        .map(|i| {
            let min = rect.min + vec2((i % cols) as f32 * size.x, (i / cols) as f32 * size.y);
            Rect::from_min_size(min, size)
        })
        .collect()
}

fn arrange(screen: Rect, cams: usize, layout: Layout) -> (Vec<Rect>, Rect) {
    if layout == Layout::Feature && cams > 1 {
        let main = Rect::from_min_size(screen.min, vec2(screen.width() * 2.0 / 3.0, screen.height()));
        let side = Rect::from_min_max(pos2(main.max.x, screen.min.y), screen.max);
        let h = side.height() / cams as f32;
        let mut rects = vec![main];
        rects.extend((0..cams).map(|i| {
            Rect::from_min_size(pos2(side.min.x, side.min.y + i as f32 * h), vec2(side.width(), h))
        }));
        let info = rects.pop().unwrap();
        return (rects, info);
    }
    let mut rects = grid(screen, cams + 1);
    let info = rects.pop().unwrap();
    (rects, info)
}

fn label(painter: &Painter, pos: Pos2, text: String, size: f32, color: Color32) {
    let galley = painter.layout_no_wrap(text, FontId::proportional(size), color);
    let bg = Rect::from_min_size(pos, galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(bg, 3.0, Color32::from_black_alpha(160));
    painter.galley(pos + vec2(5.0, 2.0), galley, color);
}

fn banner(painter: &Painter, screen: Rect, text: &str, size: f32) {
    let galley = painter.layout_no_wrap(text.into(), FontId::proportional(size), Color32::WHITE);
    let pos = pos2(
        screen.center().x - galley.size().x / 2.0,
        screen.max.y - galley.size().y - 12.0,
    );
    let bg = Rect::from_min_size(pos - vec2(8.0, 3.0), galley.size() + vec2(16.0, 6.0));
    painter.rect_filled(bg, 4.0, ALERT_RED);
    painter.galley(pos, galley, Color32::WHITE);
}

fn draw_image(painter: &Painter, texture: &TextureHandle, rect: Rect) {
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.image(texture.id(), rect, uv, Color32::WHITE);
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        ctx.set_cursor_icon(CursorIcon::None);
        ctx.request_repaint_after(Duration::from_secs(1));

        let (status, mqtt, startup_error, ready) = self.sync(ctx);
        let alerting = status
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.alert.map(|t| (i, t)))
            .max_by_key(|(_, t)| *t)
            .map(|(i, _)| i);
        if alerting.is_none() {
            self.auto_dismissed = false;
        }
        let focus = self
            .manual_focus
            .or(if self.auto_dismissed { None } else { alerting });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(root, |ui| {
                let screen = ui.max_rect();
                let ppp = ctx.pixels_per_point();
                let label_size = (screen.height() / 22.0).clamp(14.0, 48.0);
                let painter = ui.painter().clone();

                if self.tiles.is_empty() {
                    let text = match (ready, &startup_error) {
                        (_, Some(e)) => format!("Connecting to Frigate\n{e}"),
                        (true, None) => "No cameras found".to_string(),
                        (false, None) => "Connecting to Frigate".to_string(),
                    };
                    painter.text(
                        screen.center(),
                        Align2::CENTER_CENTER,
                        text,
                        FontId::proportional(label_size),
                        Color32::from_gray(180),
                    );
                    return;
                }

                let (tile_rects, info) = arrange(screen, self.tiles.len(), self.layout);
                let rects: Vec<(usize, Rect)> = match focus {
                    Some(i) => vec![(i, screen)],
                    None => tile_rects.into_iter().enumerate().collect(),
                };

                for (i, rect) in &rects {
                    let tile = &self.tiles[*i];
                    let st = &status[*i];
                    let area = fit(*rect, tile.aspect);
                    let want = (area.height() * ppp).round().clamp(120.0, 4320.0) as u32;
                    tile.want_height.store(want, Ordering::Relaxed);
                    if let Some(t) = &tile.texture {
                        draw_image(&painter, t, area);
                    }
                    if st.alert.is_some() {
                        painter.rect_stroke(*rect, 0.0, Stroke::new(4.0, ALERT_RED), StrokeKind::Inside);
                    }
                    let (text, color) = match (st.stale, &st.error) {
                        (true, Some(e)) => (format!("{}  {}", tile.name, e), ALERT_RED),
                        (true, None) => (format!("{}  stale", tile.name), ALERT_RED),
                        _ => (tile.name.clone(), Color32::WHITE),
                    };
                    label(&painter, rect.min + vec2(6.0, 6.0), text, label_size, color);

                    let response = ui.interact(*rect, ui.id().with(("tile", *i)), Sense::click());
                    if response.clicked() {
                        if focus.is_some() {
                            if self.manual_focus.is_none() {
                                self.auto_dismissed = true;
                            }
                            self.manual_focus = None;
                        } else {
                            self.manual_focus = Some(*i);
                        }
                    }
                }

                if focus.is_none() {
                    self.draw_info(&painter, info, label_size);
                }

                let text = match mqtt {
                    MqttStatus::Disabled | MqttStatus::Online => None,
                    MqttStatus::Connecting => Some("MQTT connecting"),
                    MqttStatus::Disconnected => Some("MQTT disconnected"),
                    MqttStatus::FrigateOffline => Some("Frigate offline"),
                };
                if let Some(text) = text {
                    banner(&painter, screen, text, label_size);
                }
            });
    }
}
