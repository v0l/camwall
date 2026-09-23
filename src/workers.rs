use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, ColorImage};
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use serde_json::Value;

use crate::config::{Config, MqttHost};
use crate::frigate::{self, Frigate, MqttSettings};

#[derive(Clone, Copy, PartialEq)]
pub enum MqttStatus {
    Disabled,
    Connecting,
    Online,
    FrigateOffline,
    Disconnected,
}

#[derive(Default)]
pub struct CamState {
    pub frame: Option<ColorImage>,
    pub updated: Option<Instant>,
    pub error: Option<String>,
    pub alerts: HashMap<String, Instant>,
}

pub struct Snapshot {
    pub camera: String,
    pub label: String,
    pub image: Option<ColorImage>,
    pub at: Option<DateTime<Local>>,
}

pub struct Shared {
    pub cameras: Option<Vec<(String, Arc<AtomicU32>)>>,
    pub cams: HashMap<String, CamState>,
    pub mqtt: MqttStatus,
    pub startup_error: Option<String>,
    pub snapshot: Option<Snapshot>,
}

pub type SharedRef = Arc<Mutex<Shared>>;

impl Default for Shared {
    fn default() -> Self {
        Self {
            cameras: None,
            cams: HashMap::new(),
            mqtt: MqttStatus::Disabled,
            startup_error: None,
            snapshot: None,
        }
    }
}

pub fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?
        .to_rgb8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(ColorImage::from_rgb(size, img.as_raw()))
}

fn fetch_config(frigate: &Frigate, shared: &SharedRef, ctx: &egui::Context) -> Value {
    loop {
        match frigate.config() {
            Ok(v) => {
                shared.lock().unwrap().startup_error = None;
                return v;
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
    let needs_remote = cfg.cameras.is_none() || matches!(cfg.mqtt_host, MqttHost::FromFrigate);
    let remote = needs_remote.then(|| fetch_config(&frigate, &shared, &ctx));

    let cameras = cfg.cameras.clone().unwrap_or_else(|| {
        remote.as_ref().map(frigate::dashboard_cameras).unwrap_or_default()
    });
    let remote_prefix = remote
        .as_ref()
        .and_then(|r| r["mqtt"]["topic_prefix"].as_str())
        .map(str::to_string);
    let topic_prefix = cfg
        .topic_prefix
        .clone()
        .or(remote_prefix)
        .unwrap_or_else(|| "frigate".into());
    let mqtt = match &cfg.mqtt_host {
        MqttHost::Off => None,
        MqttHost::Explicit(host, port) => Some(MqttSettings {
            host: host.clone(),
            port: *port,
            topic_prefix,
        }),
        MqttHost::FromFrigate => remote.as_ref().and_then(frigate::mqtt_settings).map(|m| MqttSettings {
            topic_prefix,
            ..m
        }),
    };

    let handles: Vec<(String, Arc<AtomicU32>)> = cameras
        .iter()
        .map(|c| (c.clone(), Arc::new(AtomicU32::new(360))))
        .collect();
    for (camera, want) in &handles {
        let (f, c, w, s, x, i) = (
            frigate.clone(),
            camera.clone(),
            want.clone(),
            shared.clone(),
            ctx.clone(),
            cfg.interval,
        );
        thread::spawn(move || poll_camera(f, c, w, s, x, i));
    }
    {
        let mut s = shared.lock().unwrap();
        s.cameras = Some(handles);
        if mqtt.is_some() {
            s.mqtt = MqttStatus::Connecting;
        }
    }
    ctx.request_repaint();
    if let Some(settings) = mqtt {
        run_mqtt(&cfg, settings, cameras, shared, ctx);
    }
}

fn poll_camera(
    frigate: Arc<Frigate>,
    camera: String,
    want_height: Arc<AtomicU32>,
    shared: SharedRef,
    ctx: egui::Context,
    interval: Duration,
) {
    loop {
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
        let pause = if failed { Duration::from_secs(5) } else { interval };
        thread::sleep(pause.saturating_sub(start.elapsed()));
    }
}

fn handle_review(shared: &SharedRef, payload: &[u8]) {
    let Ok(msg) = serde_json::from_slice::<Value>(payload) else {
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
        alerts.insert(id.to_string(), Instant::now());
    }
}

fn run_mqtt(
    cfg: &Config,
    settings: MqttSettings,
    cameras: Vec<String>,
    shared: SharedRef,
    ctx: egui::Context,
) {
    let mut opts = MqttOptions::new(
        format!("camwall-{}", std::process::id()),
        settings.host.clone(),
        settings.port,
    );
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_max_packet_size(8 << 20, 64 << 10);
    if let Some(user) = &cfg.mqtt_user {
        opts.set_credentials(user.clone(), cfg.mqtt_password.clone().unwrap_or_default());
    }
    let (client, mut connection) = Client::new(opts, 32);
    let prefix = settings.topic_prefix;
    let available = format!("{prefix}/available");
    let reviews = format!("{prefix}/reviews");
    let snapshots = format!("{prefix}/+/+/snapshot");
    let set_status = |status| {
        shared.lock().unwrap().mqtt = status;
        ctx.request_repaint();
    };

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                for t in [&available, &reviews, &snapshots] {
                    let _ = client.subscribe(t.as_str(), QoS::AtMostOnce);
                }
                set_status(MqttStatus::Online);
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let topic = p.topic.as_str();
                if topic == available {
                    set_status(if p.payload.as_ref() == b"online" {
                        MqttStatus::Online
                    } else {
                        MqttStatus::FrigateOffline
                    });
                } else if topic == reviews {
                    handle_review(&shared, &p.payload);
                    ctx.request_repaint();
                } else if let Some(rest) = topic.strip_prefix(&format!("{prefix}/")) {
                    let parts: Vec<&str> = rest.split('/').collect();
                    let [camera, label, "snapshot"] = parts[..] else {
                        continue;
                    };
                    if !cameras.iter().any(|c| c == camera) {
                        continue;
                    }
                    if let Ok(img) = decode(&p.payload) {
                        shared.lock().unwrap().snapshot = Some(Snapshot {
                            camera: camera.to_string(),
                            label: label.to_string(),
                            image: Some(img),
                            at: (!p.retain).then(Local::now),
                        });
                        ctx.request_repaint();
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("mqtt: {e}");
                set_status(MqttStatus::Disconnected);
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}
