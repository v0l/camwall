use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Id, LayerId, Order, Rect};

use crate::config::Config;

enum Output {
    Software,
    Backlight {
        path: PathBuf,
        max: u32,
        last: Option<u32>,
    },
}

pub struct Power {
    brightness: f32,
    dim: Option<(Duration, f32)>,
    off_after: Option<Duration>,
    off_command: Option<String>,
    on_command: Option<String>,
    output: Output,
    awake: Arc<AtomicBool>,
    last_activity: Instant,
    off: bool,
    level: f32,
    last_frame: Instant,
    pub woke_at: Instant,
    ignore_clicks_until: Instant,
}

fn run_command(command: &Option<String>) {
    if let Some(cmd) = command.clone() {
        thread::spawn(move || {
            if let Err(e) = Command::new("sh").arg("-c").arg(&cmd).status() {
                eprintln!("{cmd}: {e}");
            }
        });
    }
}

fn backlight(choice: Option<&str>) -> Output {
    let dir = match choice {
        Some("none") => return Output::Software,
        Some(name) => Some(PathBuf::from("/sys/class/backlight").join(name)),
        None => fs::read_dir("/sys/class/backlight")
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path()),
    };
    let Some(dir) = dir else {
        return Output::Software;
    };
    let max = fs::read_to_string(dir.join("max_brightness"))
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|m| *m > 0);
    let path = dir.join("brightness");
    let writable = fs::OpenOptions::new().write(true).open(&path).is_ok();
    match max {
        Some(max) if writable => Output::Backlight {
            path,
            max,
            last: None,
        },
        _ => {
            eprintln!(
                "{}: not writable, dimming in software instead",
                path.display()
            );
            Output::Software
        }
    }
}

impl Power {
    pub fn new(cfg: &Config, awake: Arc<AtomicBool>) -> Self {
        let now = Instant::now();
        Self {
            brightness: cfg.brightness,
            dim: cfg.dim_after.map(|d| (d, cfg.dim_brightness)),
            off_after: cfg.off_after,
            off_command: cfg.screen_off_command.clone(),
            on_command: cfg.screen_on_command.clone(),
            output: backlight(cfg.backlight.as_deref()),
            awake,
            last_activity: now,
            off: false,
            level: cfg.brightness,
            last_frame: now,
            woke_at: now,
            ignore_clicks_until: now,
        }
    }

    fn dimmed(&self, idle: Duration) -> bool {
        self.dim.is_some_and(|(after, _)| idle > after)
    }

    pub fn update(&mut self, ctx: &egui::Context, activity: bool) -> (f32, bool) {
        let now = Instant::now();
        let touched = ctx.input(|i| i.pointer.any_pressed());
        if activity || touched {
            let idle = now.duration_since(self.last_activity);
            if touched && (self.off || self.dimmed(idle)) {
                self.ignore_clicks_until = now + Duration::from_millis(800);
            }
            self.last_activity = now;
        }
        let idle = now.duration_since(self.last_activity);
        let off = self.off_after.is_some_and(|after| idle > after);
        if off != self.off {
            self.off = off;
            self.awake.store(!off, Ordering::Relaxed);
            if off {
                run_command(&self.off_command);
            } else {
                self.woke_at = now;
                run_command(&self.on_command);
            }
        }
        let target = match (off, self.dim) {
            (true, _) => 0.0,
            (false, Some((after, level))) if idle > after => level,
            _ => self.brightness,
        };
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;
        let rate = if target > self.level { 3.0 } else { 0.7 };
        let step = rate * dt;
        self.level = if (target - self.level).abs() <= step {
            target
        } else {
            self.level + step.copysign(target - self.level)
        };
        if self.level != target {
            ctx.request_repaint();
        }
        let level = self.level;
        if let Output::Backlight { path, max, last } = &mut self.output {
            let value = (level * *max as f32).round() as u32;
            if *last != Some(value) {
                if let Err(e) = fs::write(&*path, value.to_string()) {
                    eprintln!("{}: {e}", path.display());
                }
                *last = Some(value);
            }
        }
        (level, off)
    }

    pub fn clicks_allowed(&self) -> bool {
        Instant::now() >= self.ignore_clicks_until
    }

    pub fn paint(&self, ctx: &egui::Context, screen: Rect, level: f32) {
        if matches!(self.output, Output::Backlight { .. }) || level >= 0.999 {
            return;
        }
        let alpha = ((1.0 - level.clamp(0.0, 1.0)) * 255.0).round() as u8;
        ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("dim")))
            .rect_filled(screen, 0.0, Color32::from_black_alpha(alpha));
    }
}
