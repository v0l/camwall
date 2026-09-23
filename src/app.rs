use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{
    self, epaint::RectShape, pos2, vec2, Align2, Color32, ColorImage, CursorIcon, FontId, Painter,
    Pos2, Rect, Sense, Shadow, Stroke, StrokeKind, TextureHandle, TextureOptions,
};

use crate::config::{Config, Layout};
use crate::power::Power;
use crate::stats::{self, FrigateStats, HostStats};
use crate::workers::{self, Link, SharedRef};

const STALE_AFTER: Duration = Duration::from_secs(10);
const ALERT_TIMEOUT: Duration = Duration::from_secs(600);
const SEVERITY_ALERT: Color32 = Color32::from_rgb(0x99, 0x1b, 0x1b);
const WARNING: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);
const GAP: f32 = 4.0;
const RADIUS: u8 = 10;
const EVENT_TIME_FORMAT: &str = "%H:%M:%S";

struct Tile {
    name: String,
    want_height: Arc<AtomicU32>,
    texture: Option<TextureHandle>,
    aspect: f32,
}

struct TileStatus {
    stale: bool,
    error: Option<String>,
    alert_seen: Option<Instant>,
    alert_start: Option<f64>,
    active: bool,
}

struct EventView {
    id: String,
    caption: String,
    start: f64,
    texture: TextureHandle,
    aspect: f32,
}

pub struct App {
    shared: SharedRef,
    event_height: Arc<AtomicU32>,
    layout: Layout,
    clock_format: String,
    date_format: String,
    tiles: Vec<Tile>,
    events: Vec<EventView>,
    events_version: u64,
    manual_focus: Option<usize>,
    auto_dismissed: bool,
    power: Power,
    diagnostics: bool,
    host: Option<HostStats>,
    frigate_stats: Option<FrigateStats>,
}

fn local_time(epoch: f64) -> Option<DateTime<Local>> {
    let secs = epoch.floor() as i64;
    let nanos = ((epoch - epoch.floor()) * 1e9) as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

impl App {
    pub fn new(cc: &eframe::CreationContext, cfg: Config) -> Self {
        let shared = SharedRef::default();
        let (awake, event_height) = {
            let s = shared.lock().unwrap();
            (s.awake.clone(), s.event_height.clone())
        };
        let power = Power::new(&cfg, awake);
        let layout = cfg.layout;
        let diagnostics = cfg.diagnostics;
        let clock_format = cfg.clock_format.clone();
        let date_format = cfg.date_format.clone();
        let (s, ctx, cfg) = (shared.clone(), cc.egui_ctx.clone(), Arc::new(cfg));
        thread::spawn(move || workers::start(cfg, s, ctx));
        Self {
            shared,
            event_height,
            layout,
            clock_format,
            date_format,
            tiles: Vec::new(),
            events: Vec::new(),
            events_version: 0,
            manual_focus: None,
            auto_dismissed: false,
            power,
            diagnostics,
            host: None,
            frigate_stats: None,
        }
    }

    fn format_event_time(&self, epoch: f64) -> String {
        let Some(t) = local_time(epoch) else {
            return String::new();
        };
        if t.date_naive() == Local::now().date_naive() {
            t.format(EVENT_TIME_FORMAT).to_string()
        } else {
            format!(
                "{}  {}",
                t.format(&self.date_format),
                t.format(EVENT_TIME_FORMAT)
            )
        }
    }

    fn sync(&mut self, ctx: &egui::Context) -> (Vec<TileStatus>, Link, Option<String>, bool) {
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
            cam.alerts
                .retain(|_, a| now.duration_since(a.seen) < ALERT_TIMEOUT);
            if let Some(img) = cam.frame.take() {
                tile.aspect = img.size[0] as f32 / img.size[1] as f32;
                upload(ctx, &mut tile.texture, &tile.name, img);
            }
            let fresh_since = cam
                .updated
                .map_or(self.power.woke_at, |u| u.max(self.power.woke_at));
            let latest = cam.alerts.values().max_by_key(|a| a.seen);
            status.push(TileStatus {
                stale: now.duration_since(fresh_since) > STALE_AFTER,
                error: cam.error.clone(),
                alert_seen: latest.map(|a| a.seen),
                alert_start: latest.map(|a| a.start),
                active: cam.active_objects > 0,
            });
        }
        if s.events_version != self.events_version {
            self.events_version = s.events_version;
            let mut old = std::mem::take(&mut self.events);
            for entry in s.events.iter_mut() {
                let previous = old
                    .iter()
                    .position(|v| v.id == entry.id)
                    .map(|i| old.swap_remove(i));
                let mut texture = previous.as_ref().map(|v| v.texture.clone());
                let mut aspect = previous.as_ref().map_or(1.0, |v| v.aspect);
                if let Some(img) = entry.image.take() {
                    aspect = img.size[0] as f32 / img.size[1] as f32;
                    upload(ctx, &mut texture, &entry.id, img);
                }
                if let Some(texture) = texture {
                    self.events.push(EventView {
                        id: entry.id.clone(),
                        caption: format!("{}  {}", entry.label, entry.camera),
                        start: entry.start,
                        texture,
                        aspect,
                    });
                }
            }
        }
        self.host = s.host.clone();
        self.frigate_stats = s.frigate_stats.clone();
        (status, s.link, s.startup_error.clone(), ready)
    }

    fn draw_info(&self, painter: &Painter, info: Rect, label_size: f32, ppp: f32) {
        let local = Local::now();
        let clock = local.format(&self.clock_format).to_string();
        let date = local.format(&self.date_format).to_string();
        let header_bottom = if self.events.is_empty() {
            let clock_size = info.height() / 4.0;
            let top = info.min.y + info.height() * 0.05;
            painter.text(
                pos2(info.center().x, top),
                Align2::CENTER_TOP,
                clock,
                FontId::proportional(clock_size),
                Color32::from_gray(225),
            );
            painter.text(
                pos2(info.center().x, top + clock_size * 1.1),
                Align2::CENTER_TOP,
                date,
                FontId::proportional(label_size),
                Color32::from_gray(150),
            );
            top + clock_size * 1.1 + label_size * 1.6
        } else {
            let clock_size = (info.height() / 7.0).max(label_size * 1.4);
            let clock = painter.layout_no_wrap(
                clock,
                FontId::proportional(clock_size),
                Color32::from_gray(225),
            );
            let date = painter.layout_no_wrap(
                date,
                FontId::proportional(label_size),
                Color32::from_gray(150),
            );
            let gap = label_size;
            let width = clock.size().x + gap + date.size().x;
            let left = info.center().x - width / 2.0;
            let top = info.min.y + 4.0;
            let date_top = top + clock.size().y - date.size().y - clock_size * 0.1;
            let bottom = top + clock.size().y;
            painter.galley(pos2(left, top), clock.clone(), Color32::from_gray(225));
            painter.galley(
                pos2(left + clock.size().x + gap, date_top),
                date,
                Color32::from_gray(150),
            );
            bottom + 6.0
        };
        let caption_size = label_size * 0.8;
        let diag_size = label_size * 0.6;
        let mut lines: Vec<(String, Color32)> = Vec::new();
        if self.diagnostics {
            if let Some(h) = &self.host {
                let color = if h.undervoltage {
                    WARNING
                } else {
                    Color32::from_gray(130)
                };
                lines.push((stats::host_line(h), color));
            }
            if let Some(f) = &self.frigate_stats {
                lines.push((stats::frigate_line(f), Color32::from_gray(130)));
            }
        }
        let diag_top = info.max.y - lines.len() as f32 * diag_size * 1.3 - 4.0;
        for (i, (text, color)) in lines.into_iter().enumerate() {
            let pos = pos2(info.center().x, diag_top + i as f32 * diag_size * 1.3);
            fitted_text(painter, pos, text, diag_size, info.width() - 16.0, color);
        }
        let slot = Rect::from_min_max(
            pos2(info.min.x + 8.0, header_bottom),
            pos2(info.max.x - 8.0, diag_top - 4.0),
        );
        self.draw_events(painter, slot, caption_size, ppp);
    }

    fn draw_events(&self, painter: &Painter, slot: Rect, caption_size: f32, ppp: f32) {
        if self.events.is_empty() || slot.height() < 24.0 {
            return;
        }
        let aspect = self.events.iter().map(|e| e.aspect).sum::<f32>() / self.events.len() as f32;
        let image_rect = |cell: Rect, caption: f32| {
            let inner = cell.shrink(3.0);
            Rect::from_min_max(inner.min, pos2(inner.max.x, inner.max.y - caption * 1.3))
        };
        let caption_for = |cell: Rect| caption_size.min(cell.height() * 0.14);
        let min_height = (slot.height() * 0.5).max(56.0);
        let min_width = caption_size * 8.0;
        let count = (1..=self.events.len())
            .rev()
            .find(|&k| {
                let cell = cells(slot, k, aspect)[0];
                let image = fit(image_rect(cell, caption_for(cell)), aspect);
                image.height() >= min_height && cell.width() >= min_width
            })
            .unwrap_or(1);
        let layout = cells(slot, count, aspect);
        let mut want = 64.0f32;
        for (event, cell) in self.events.iter().zip(layout) {
            let caption = caption_for(cell);
            let area = fit(image_rect(cell, caption), event.aspect);
            want = want.max(area.height() * ppp);
            paint_texture(painter, &event.texture, area);
            let text = format!("{}  {}", event.caption, self.format_event_time(event.start));
            let pos = pos2(cell.center().x, area.max.y + caption * 0.2);
            let color = Color32::from_gray(200);
            fitted_text(painter, pos, text, caption, cell.width() - 6.0, color);
        }
        self.event_height
            .store(want.round() as u32, Ordering::Relaxed);
    }
}

fn fitted_text(painter: &Painter, pos: Pos2, text: String, size: f32, width: f32, color: Color32) {
    let natural = painter
        .layout_no_wrap(text.clone(), FontId::proportional(size), color)
        .size()
        .x;
    let size = size * width.min(natural) / natural.max(1.0);
    painter.text(
        pos,
        Align2::CENTER_TOP,
        text,
        FontId::proportional(size),
        color,
    );
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

fn columns(area: Rect, slots: usize, aspect: f32) -> usize {
    let shape = |cols: usize| {
        let rows = slots.div_ceil(cols);
        vec2(area.width() / cols as f32, area.height() / rows as f32)
    };
    let score = |cols: usize| {
        let cell = shape(cols);
        let fitted = fit(Rect::from_min_size(Pos2::ZERO, cell), aspect).area();
        let mismatch = ((cell.x / cell.y) / aspect).ln().abs();
        (fitted, mismatch)
    };
    (1..=slots.max(1))
        .max_by(|&a, &b| {
            let ((area_a, mis_a), (area_b, mis_b)) = (score(a), score(b));
            if (area_a - area_b).abs() > area_a.max(area_b) * 0.01 {
                area_a.total_cmp(&area_b)
            } else {
                mis_b.total_cmp(&mis_a)
            }
        })
        .unwrap_or(1)
}

fn cell_size(area: Rect, slots: usize, cols: usize) -> egui::Vec2 {
    let rows = slots.div_ceil(cols).max(1);
    vec2(area.width() / cols as f32, area.height() / rows as f32)
}

fn cells(area: Rect, count: usize, aspect: f32) -> Vec<Rect> {
    let cols = columns(area, count, aspect);
    let cell = cell_size(area, count, cols);
    let last_row = (count - 1) / cols;
    let in_last = count - last_row * cols;
    (0..count)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            let offset = if row == last_row {
                (cols - in_last) as f32 * cell.x / 2.0
            } else {
                0.0
            };
            let min = area.min + vec2(offset + col as f32 * cell.x, row as f32 * cell.y);
            Rect::from_min_size(min, cell)
        })
        .collect()
}

fn grid(screen: Rect, cams: usize, aspect: f32) -> (Vec<Rect>, Rect) {
    let slots = cams + 1;
    let cols = columns(screen, slots, aspect);
    let cell = cell_size(screen, slots, cols);
    let at = |i: usize| screen.min + vec2((i % cols) as f32 * cell.x, (i / cols) as f32 * cell.y);
    let rects = (0..cams)
        .map(|i| Rect::from_min_size(at(i), cell))
        .collect();
    let info = Rect::from_min_max(at(cams), pos2(screen.max.x, at(cams).y + cell.y));
    (rects, info)
}

fn arrange(screen: Rect, cams: usize, layout: Layout, aspect: f32) -> (Vec<Rect>, Rect) {
    if layout == Layout::Feature && cams > 1 {
        let main = Rect::from_min_size(
            screen.min,
            vec2(screen.width() * 2.0 / 3.0, screen.height()),
        );
        let side = Rect::from_min_max(pos2(main.max.x, screen.min.y), screen.max);
        let h = side.height() / cams as f32;
        let mut rects = vec![main];
        rects.extend((0..cams).map(|i| {
            Rect::from_min_size(
                pos2(side.min.x, side.min.y + i as f32 * h),
                vec2(side.width(), h),
            )
        }));
        let info = rects.pop().unwrap();
        return (rects, info);
    }
    grid(screen, cams, aspect)
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
    painter.rect_filled(bg, 4.0, WARNING);
    painter.galley(pos, galley, Color32::WHITE);
}

fn paint_texture(painter: &Painter, texture: &TextureHandle, rect: Rect) {
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let shape = RectShape::filled(rect, RADIUS, Color32::WHITE).with_texture(texture.id(), uv);
    painter.add(shape);
}

fn outline(painter: &Painter, rect: Rect) {
    let glow = Shadow {
        offset: [0, 0],
        blur: 16,
        spread: 1,
        color: SEVERITY_ALERT,
    };
    painter.add(glow.as_shape(rect, RADIUS));
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        ctx.set_cursor_icon(CursorIcon::None);
        ctx.request_repaint_after(Duration::from_secs(1));

        let (status, link, startup_error, ready) = self.sync(ctx);
        let activity = status.iter().any(|s| s.active || s.alert_seen.is_some());
        let (level, asleep) = self.power.update(ctx, activity);
        let screen = root.max_rect();

        let any_alert = status.iter().any(|s| s.alert_seen.is_some());
        let auto: Vec<usize> = if any_alert {
            (0..status.len())
                .filter(|&i| status[i].alert_seen.is_some() || status[i].active)
                .collect()
        } else {
            Vec::new()
        };
        if auto.is_empty() {
            self.auto_dismissed = false;
        }
        let focus: Vec<usize> = match self.manual_focus {
            Some(i) => vec![i],
            None if !self.auto_dismissed => auto,
            None => Vec::new(),
        };
        let clicks_allowed = self.power.clicks_allowed();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(root, |ui| {
                if asleep {
                    return;
                }
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

                let known: Vec<f32> = self
                    .tiles
                    .iter()
                    .map(|t| t.aspect)
                    .filter(|a| *a > 0.0)
                    .collect();
                let aspect = if known.is_empty() {
                    16.0 / 9.0
                } else {
                    known.iter().sum::<f32>() / known.len() as f32
                };
                let (tile_rects, info) = arrange(screen, self.tiles.len(), self.layout, aspect);
                let rects: Vec<(usize, Rect)> = match focus.len() {
                    0 => tile_rects.into_iter().enumerate().collect(),
                    1 => vec![(focus[0], screen)],
                    n => focus
                        .iter()
                        .copied()
                        .zip(cells(screen, n, aspect))
                        .collect(),
                };

                for (i, rect) in &rects {
                    let tile = &self.tiles[*i];
                    let st = &status[*i];
                    let area = fit(rect.shrink(GAP), tile.aspect);
                    let want = (area.height() * ppp).round().clamp(120.0, 4320.0) as u32;
                    tile.want_height.store(want, Ordering::Relaxed);
                    let highlighted = st.active || st.alert_seen.is_some();
                    if highlighted {
                        outline(&painter, area);
                    }
                    if let Some(t) = &tile.texture {
                        paint_texture(&painter, t, area);
                    }
                    if highlighted {
                        painter.rect_stroke(
                            area,
                            RADIUS,
                            Stroke::new(3.0, SEVERITY_ALERT),
                            StrokeKind::Inside,
                        );
                    }
                    let (text, color) = match (st.stale, &st.error, st.alert_start) {
                        (true, Some(e), _) => (format!("{}  {}", tile.name, e), WARNING),
                        (true, None, _) => (format!("{}  stale", tile.name), WARNING),
                        (false, _, Some(start)) => (
                            format!("{}  {}", tile.name, self.format_event_time(start)),
                            Color32::WHITE,
                        ),
                        _ => (tile.name.clone(), Color32::WHITE),
                    };
                    label(&painter, area.min + vec2(8.0, 8.0), text, label_size, color);

                    let response = ui.interact(*rect, ui.id().with(("tile", *i)), Sense::click());
                    if response.clicked() && clicks_allowed {
                        if self.manual_focus.is_some() {
                            self.manual_focus = None;
                        } else if focus.len() == 1 {
                            self.auto_dismissed = true;
                        } else {
                            self.manual_focus = Some(*i);
                        }
                    }
                }

                if focus.is_empty() {
                    self.draw_info(&painter, info, label_size, ppp);
                }

                if link == Link::Offline {
                    banner(&painter, screen, "Frigate disconnected", label_size);
                }
            });
        self.power.paint(ctx, screen, level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(cams: usize, w: f32, h: f32, aspect: f32) -> usize {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(w, h));
        let (rects, _) = grid(screen, cams, aspect);
        let mut xs: Vec<i32> = rects.iter().map(|r| r.min.x as i32).collect();
        xs.sort();
        xs.dedup();
        xs.len()
    }

    #[test]
    fn grid_columns() {
        assert_eq!(columns(3, 1280.0, 720.0, 16.0 / 9.0), 2);
        assert_eq!(columns(4, 1280.0, 720.0, 16.0 / 9.0), 3);
        assert_eq!(columns(8, 1280.0, 720.0, 16.0 / 9.0), 3);
        assert_eq!(columns(3, 720.0, 1280.0, 16.0 / 9.0), 1);
    }

    #[test]
    fn split_centres_last_row() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 720.0));
        assert_eq!(cells(screen, 1, 16.0 / 9.0), vec![screen]);
        let three = cells(screen, 3, 16.0 / 9.0);
        assert_eq!(three.len(), 3);
        assert!((three[2].center().x - 640.0).abs() < 1.0);
        assert!(three.iter().all(|r| screen.contains_rect(*r)));
    }

    #[test]
    fn info_fills_last_row() {
        let screen = Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 720.0));
        let (rects, info) = grid(screen, 4, 16.0 / 9.0);
        assert_eq!(rects.len(), 4);
        assert_eq!(info.max.x, 1280.0);
        assert_eq!(info.max.y, 720.0);
        assert!(info.width() > 800.0);
    }
}
