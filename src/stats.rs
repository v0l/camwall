use std::fs;
use std::thread;
use std::time::Duration;

use eframe::egui;
use serde_json::Value;

use crate::workers::SharedRef;

#[derive(Clone, Default)]
pub struct HostStats {
    pub name: String,
    pub cpu: Option<f32>,
    pub temp: Option<f32>,
    pub load: Option<f32>,
    pub mem: Option<f32>,
    pub undervoltage: bool,
}

#[derive(Clone, Default)]
pub struct FrigateStats {
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub detectors: Vec<(String, f32)>,
    pub temps: Vec<(String, f32)>,
    pub disk: Option<f32>,
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn cpu_times() -> Option<(u64, u64)> {
    let stat = read("/proc/stat")?;
    let fields: Vec<u64> = stat
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let idle = fields.get(3)? + fields.get(4).unwrap_or(&0);
    Some((fields.iter().sum(), idle))
}

fn mem_used() -> Option<f32> {
    let info = read("/proc/meminfo")?;
    let field = |key: &str| -> Option<f32> {
        info.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")?;
    Some(100.0 * (1.0 - field("MemAvailable:")? / total))
}

fn hwmon(name: &str, file: &str) -> Option<String> {
    fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| read(&format!("{}/name", p.display())).as_deref() == Some(name))
        .and_then(|p| read(&format!("{}/{file}", p.display())))
}

fn cpu_temp() -> Option<f32> {
    let milli = hwmon("cpu_thermal", "temp1_input")
        .or_else(|| read("/sys/class/thermal/thermal_zone0/temp"))?;
    milli.parse::<f32>().ok().map(|m| m / 1000.0)
}

pub fn sample_host(shared: SharedRef, ctx: egui::Context) {
    let name = read("/proc/sys/kernel/hostname").unwrap_or_else(|| "display".into());
    let mut last = cpu_times();
    loop {
        thread::sleep(Duration::from_secs(3));
        let now = cpu_times();
        let cpu = match (last, now) {
            (Some((t0, i0)), Some((t1, i1))) if t1 > t0 => {
                Some(100.0 * (1.0 - (i1 - i0) as f32 / (t1 - t0) as f32))
            }
            _ => None,
        };
        last = now;
        let stats = HostStats {
            name: name.clone(),
            cpu,
            temp: cpu_temp(),
            load: read("/proc/loadavg").and_then(|l| l.split_whitespace().next()?.parse().ok()),
            mem: mem_used(),
            undervoltage: hwmon("rpi_volt", "in0_lcrit_alarm").as_deref() == Some("1"),
        };
        shared.lock().unwrap().host = Some(stats);
        ctx.request_repaint();
    }
}

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f as f32),
        Value::String(s) => s.trim().trim_end_matches('%').parse().ok(),
        _ => None,
    }
    .filter(|f: &f32| f.is_finite() && *f >= 0.0)
}

pub fn parse_frigate(stats: &Value) -> FrigateStats {
    let gpu = stats["gpu_usages"]
        .as_object()
        .and_then(|g| g.values().next());
    let mut detectors: Vec<(String, f32)> = stats["detectors"]
        .as_object()
        .map(|d| {
            d.iter()
                .filter_map(|(name, v)| Some((name.clone(), number(&v["inference_speed"])?)))
                .collect()
        })
        .unwrap_or_default();
    detectors.sort_by(|a, b| a.0.cmp(&b.0));
    let mut temps: Vec<(String, f32)> = stats["service"]["temperatures"]
        .as_object()
        .map(|t| {
            t.iter()
                .filter_map(|(name, v)| Some((name.clone(), number(v)?)))
                .collect()
        })
        .unwrap_or_default();
    temps.sort_by(|a, b| a.0.cmp(&b.0));
    let storage = &stats["service"]["storage"]["/media/frigate/recordings"];
    let disk = match (number(&storage["used"]), number(&storage["total"])) {
        (Some(used), Some(total)) if total > 0.0 => Some(100.0 * used / total),
        _ => None,
    };
    FrigateStats {
        cpu: number(&stats["cpu_usages"]["frigate.full_system"]["cpu"]),
        gpu: gpu.and_then(|g| number(&g["gpu"])),
        gpu_temp: gpu.and_then(|g| number(&g["temp"])),
        detectors,
        temps,
        disk,
    }
}

pub fn host_line(h: &HostStats) -> String {
    let mut parts = vec![h.name.clone()];
    if let Some(v) = h.cpu {
        parts.push(format!("CPU {v:.0}%"));
    }
    if let Some(v) = h.temp {
        parts.push(format!("{v:.0}°C"));
    }
    if let Some(v) = h.load {
        parts.push(format!("load {v:.2}"));
    }
    if let Some(v) = h.mem {
        parts.push(format!("mem {v:.0}%"));
    }
    if h.undervoltage {
        parts.push("UNDERVOLTAGE".into());
    }
    parts.join("  ")
}

pub fn frigate_line(f: &FrigateStats) -> String {
    let mut parts = vec!["Frigate".to_string()];
    if let Some(v) = f.cpu {
        parts.push(format!("CPU {v:.0}%"));
    }
    match (f.gpu, f.gpu_temp) {
        (Some(g), Some(t)) => parts.push(format!("GPU {g:.0}% {t:.0}°C")),
        (Some(g), None) => parts.push(format!("GPU {g:.0}%")),
        _ => {}
    }
    for (name, ms) in &f.detectors {
        parts.push(format!("{name} {ms:.1} ms"));
    }
    for (name, t) in &f.temps {
        parts.push(format!("{name} {t:.0}°C"));
    }
    if let Some(v) = f.disk {
        parts.push(format!("disk {v:.0}%"));
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frigate_stats_line() {
        let stats: Value = serde_json::from_str(
            r#"{"cpu_usages":{"frigate.full_system":{"cpu":"10.0"}},
                "gpu_usages":{"gpu0":{"gpu":"15.0%","temp":"56.0"}},
                "detectors":{"onnx":{"inference_speed":8.94}},
                "service":{"temperatures":{"apex_0":44.4},
                           "storage":{"/media/frigate/recordings":{"used":250.0,"total":1000.0}}}}"#,
        )
        .unwrap();
        assert_eq!(
            frigate_line(&parse_frigate(&stats)),
            "Frigate  CPU 10%  GPU 15% 56°C  onnx 8.9 ms  apex_0 44°C  disk 25%"
        );
    }

    #[test]
    fn missing_fields_are_skipped() {
        let stats: Value = serde_json::from_str(r#"{"gpu_usages":{"x":{"gpu":"-1.0%"}}}"#).unwrap();
        assert_eq!(frigate_line(&parse_frigate(&stats)), "Frigate");
    }
}
