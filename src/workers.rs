use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage};
use serde_json::Value;
use tungstenite::Message;

use crate::config::Config;
use crate::frigate::{self, Frigate};
use crate::stats::{self, FrigateStats, HostStats};

#[derive(Clone, Copy, PartialEq)]
pub enum Link {
    Connecting,
    Online,
    Offline,
}

pub struct Alert {
    pub seen: Instant,
    pub start: f64,
}

#[derive(Default)]
pub struct CamState {
    pub frame: Option<ColorImage>,
    pub updated: Option<Instant>,
    pub error: Option<String>,
    pub alerts: HashMap<String, Alert>,
    pub active_objects: u32,
}

pub struct LastEvent {
    pub camera: String,
    pub label: String,
    pub start: f64,
    pub image: Option<ColorImage>,
}

pub struct Shared {
    pub cameras: Option<Vec<(String, Arc<AtomicU32>)>>,
    pub cams: HashMap<String, CamState>,
    pub link: Link,
    pub startup_error: Option<String>,
    pub last_event: Option<LastEvent>,
    pub awake: Arc<AtomicBool>,
    pub event_height: Arc<AtomicU32>,
    pub host: Option<HostStats>,
    pub frigate_stats: Option<FrigateStats>,
}

pub type SharedRef = Arc<Mutex<Shared>>;

impl Default for Shared {
    fn default() -> Self {
        Self {
            cameras: None,
            cams: HashMap::new(),
            link: Link::Connecting,
            startup_error: None,
            last_event: None,
            awake: Arc::new(AtomicBool::new(true)),
            event_height: Arc::new(AtomicU32::new(240)),
            host: None,
            frigate_stats: None,
        }
    }
}

struct EventRef {
    id: String,
    camera: String,
    label: String,
    start: f64,
    snapshot: bool,
}

pub fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?
        .to_rgb8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(ColorImage::from_rgb(size, img.as_raw()))
}

fn discover_cameras(frigate: &Frigate, shared: &SharedRef, ctx: &egui::Context) -> Vec<String> {
    loop {
        match frigate.config() {
            Ok(v) => {
                shared.lock().unwrap().startup_error = None;
                return frigate::dashboard_cameras(&v);
            }
            Err(e) => {
                eprintln!("frigate config: {e}");
                shared.lock().unwrap().startup_error = Some(e);
                ctx.request_repaint();
                thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

pub fn start(cfg: Arc<Config>, shared: SharedRef, ctx: egui::Context) {
    let frigate = Arc::new(Frigate::new(&cfg));
    let cameras = match &cfg.cameras {
        Some(c) => c.clone(),
        None => discover_cameras(&frigate, &shared, &ctx),
    };

    let handles: Vec<(String, Arc<AtomicU32>)> = cameras
        .iter()
        .map(|c| (c.clone(), Arc::new(AtomicU32::new(360))))
        .collect();
    let awake = shared.lock().unwrap().awake.clone();
    for (camera, want) in &handles {
        let (f, c, w, a, s, x, i) = (
            frigate.clone(),
            camera.clone(),
            want.clone(),
            awake.clone(),
            shared.clone(),
            ctx.clone(),
            cfg.interval,
        );
        thread::spawn(move || poll_camera(f, c, w, a, s, x, i));
    }
    shared.lock().unwrap().cameras = Some(handles);
    ctx.request_repaint();

    let (tx, rx) = mpsc::channel();
    {
        let (f, s, x) = (frigate.clone(), shared.clone(), ctx.clone());
        thread::spawn(move || fetch_events(f, rx, s, x));
    }
    if cfg.diagnostics {
        let (s, x) = (shared.clone(), ctx.clone());
        thread::spawn(move || stats::sample_host(s, x));
        match frigate.stats() {
            Ok(v) => shared.lock().unwrap().frigate_stats = Some(stats::parse_frigate(&v)),
            Err(e) => eprintln!("frigate stats: {e}"),
        }
    }
    match frigate.latest_event(&cameras) {
        Ok(Some(event)) => {
            if let Some(e) = event_ref(&event) {
                let _ = tx.send(e);
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("frigate events: {e}"),
    }
    run_live(&frigate, &cameras, tx, shared, ctx);
}

fn poll_camera(
    frigate: Arc<Frigate>,
    camera: String,
    want_height: Arc<AtomicU32>,
    awake: Arc<AtomicBool>,
    shared: SharedRef,
    ctx: egui::Context,
    interval: Duration,
) {
    loop {
        if !awake.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let start = Instant::now();
        let result = frigate
            .snapshot(&camera, want_height.load(Ordering::Relaxed))
            .and_then(|b| decode(&b));
        let failed = result.is_err();
        {
            let mut s = shared.lock().unwrap();
            let cam = s.cams.entry(camera.clone()).or_default();
            match result {
                Ok(img) => {
                    cam.frame = Some(img);
                    cam.updated = Some(Instant::now());
                    cam.error = None;
                }
                Err(e) => cam.error = Some(e),
            }
        }
        ctx.request_repaint();
        let pause = if failed {
            Duration::from_secs(5)
        } else {
            interval
        };
        thread::sleep(pause.saturating_sub(start.elapsed()));
    }
}

fn event_ref(event: &Value) -> Option<EventRef> {
    Some(EventRef {
        id: event["id"].as_str()?.to_string(),
        camera: event["camera"].as_str()?.to_string(),
        label: event["label"].as_str()?.to_string(),
        start: event["start_time"].as_f64()?,
        snapshot: event["has_snapshot"].as_bool() == Some(true),
    })
}

fn fetch_events(
    frigate: Arc<Frigate>,
    rx: Receiver<EventRef>,
    shared: SharedRef,
    ctx: egui::Context,
) {
    while let Ok(mut event) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            event = newer;
        }
        let height = shared.lock().unwrap().event_height.load(Ordering::Relaxed);
        match frigate
            .event_image(&event.id, event.snapshot, height)
            .and_then(|b| decode(&b))
        {
            Ok(img) => {
                shared.lock().unwrap().last_event = Some(LastEvent {
                    camera: event.camera,
                    label: event.label,
                    start: event.start,
                    image: Some(img),
                });
                ctx.request_repaint();
            }
            Err(e) => eprintln!("event {}: {e}", event.id),
        }
    }
}

fn parse_payload(payload: &Value) -> Option<Value> {
    match payload {
        Value::String(s) => serde_json::from_str(s).ok(),
        other => Some(other.clone()),
    }
}

fn handle_activity(shared: &SharedRef, cameras: &[String], payload: &Value) {
    let Some(activity) = parse_payload(payload) else {
        return;
    };
    let mut s = shared.lock().unwrap();
    for camera in cameras {
        let Some(objects) = activity[camera]["objects"].as_array() else {
            continue;
        };
        let active = objects
            .iter()
            .filter(|o| o["stationary"].as_bool() != Some(true))
            .count();
        s.cams.entry(camera.clone()).or_default().active_objects = active as u32;
    }
}

fn handle_review(shared: &SharedRef, payload: &Value) {
    let Some(msg) = parse_payload(payload) else {
        return;
    };
    let after = &msg["after"];
    let (Some(camera), Some(id)) = (after["camera"].as_str(), after["id"].as_str()) else {
        return;
    };
    let ended = msg["type"] == "end" || !after["end_time"].is_null();
    let alert = after["severity"] == "alert";
    let mut s = shared.lock().unwrap();
    let alerts = &mut s.cams.entry(camera.to_string()).or_default().alerts;
    if ended || !alert {
        alerts.remove(id);
    } else {
        let start = after["start_time"].as_f64().unwrap_or(0.0);
        alerts.insert(
            id.to_string(),
            Alert {
                seen: Instant::now(),
                start,
            },
        );
    }
}

struct EventTracker {
    last_id: Option<String>,
    last_fetch: Instant,
}

impl EventTracker {
    fn handle(&mut self, cameras: &[String], payload: &Value, tx: &Sender<EventRef>) {
        let Some(msg) = parse_payload(payload) else {
            return;
        };
        let after = &msg["after"];
        let wanted = after["camera"]
            .as_str()
            .is_some_and(|c| cameras.iter().any(|x| x == c));
        if !wanted || after["false_positive"].as_bool() == Some(true) {
            return;
        }
        let Some(event) = event_ref(after) else {
            return;
        };
        let is_new = self.last_id.as_deref() != Some(event.id.as_str());
        let due = self.last_fetch.elapsed() > Duration::from_secs(3);
        if is_new || msg["type"] == "end" || due {
            self.last_id = Some(event.id.clone());
            self.last_fetch = Instant::now();
            let _ = tx.send(event);
        }
    }
}

fn run_live(
    frigate: &Frigate,
    cameras: &[String],
    tx: Sender<EventRef>,
    shared: SharedRef,
    ctx: egui::Context,
) {
    let set_link = |link| {
        shared.lock().unwrap().link = link;
        ctx.request_repaint();
    };
    let mut tracker = EventTracker {
        last_id: None,
        last_fetch: Instant::now(),
    };
    loop {
        let mut socket = match frigate.connect() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("frigate websocket: {e}");
                set_link(Link::Offline);
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let hello = r#"{"topic":"onConnect","message":"","retain":false}"#;
        if socket.send(Message::text(hello)).is_err() {
            continue;
        }
        set_link(Link::Online);
        loop {
            let text = match socket.read() {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("frigate websocket: {e}");
                    break;
                }
            };
            let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            let Some(topic) = msg["topic"].as_str() else {
                continue;
            };
            let payload = &msg["payload"];
            match topic {
                "camera_activity" => handle_activity(&shared, cameras, payload),
                "reviews" => handle_review(&shared, payload),
                "stats" => {
                    if let Some(v) = parse_payload(payload) {
                        shared.lock().unwrap().frigate_stats = Some(stats::parse_frigate(&v));
                    }
                }
                "events" => {
                    tracker.handle(cameras, payload, &tx);
                    continue;
                }
                t => {
                    let Some(camera) = t.strip_suffix("/all/active") else {
                        continue;
                    };
                    if !cameras.iter().any(|c| c == camera) {
                        continue;
                    }
                    let count = parse_payload(payload).and_then(|v| v.as_u64()).unwrap_or(0);
                    shared
                        .lock()
                        .unwrap()
                        .cams
                        .entry(camera.to_string())
                        .or_default()
                        .active_objects = count as u32;
                }
            }
            ctx.request_repaint();
        }
        set_link(Link::Offline);
        thread::sleep(Duration::from_secs(2));
    }
}
