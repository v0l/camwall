use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    Grid,
    Feature,
}

pub enum MqttHost {
    FromFrigate,
    Off,
    Explicit(String, u16),
}

pub struct Config {
    pub frigate_url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub insecure: bool,
    pub cameras: Option<Vec<String>>,
    pub interval: Duration,
    pub layout: Layout,
    pub mqtt_host: MqttHost,
    pub mqtt_user: Option<String>,
    pub mqtt_password: Option<String>,
    pub topic_prefix: Option<String>,
    pub clock_format: String,
    pub date_format: String,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse_host(value: &str) -> MqttHost {
    if value.eq_ignore_ascii_case("off") {
        return MqttHost::Off;
    }
    match value.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            MqttHost::Explicit(host.to_string(), port.parse().unwrap())
        }
        _ => MqttHost::Explicit(value.to_string(), 1883),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let frigate_url = var("FRIGATE_URL")
            .ok_or("FRIGATE_URL is required, e.g. https://frigate.local:8971")?
            .trim_end_matches('/')
            .to_string();
        let interval = var("INTERVAL")
            .map(|v| v.parse::<f32>().map_err(|_| format!("INTERVAL: bad number {v:?}")))
            .transpose()?
            .unwrap_or(0.5);
        let layout = match var("LAYOUT").as_deref() {
            None | Some("grid") => Layout::Grid,
            Some("feature") => Layout::Feature,
            Some(other) => return Err(format!("LAYOUT: expected grid or feature, got {other:?}")),
        };
        Ok(Self {
            frigate_url,
            user: var("FRIGATE_USER"),
            password: var("FRIGATE_PASSWORD"),
            insecure: matches!(var("FRIGATE_INSECURE").as_deref(), Some("1" | "true" | "yes")),
            cameras: var("CAMERAS").map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }),
            interval: Duration::from_secs_f32(interval.max(0.05)),
            layout,
            mqtt_host: var("MQTT_HOST").map_or(MqttHost::FromFrigate, |v| parse_host(&v)),
            mqtt_user: var("MQTT_USER"),
            mqtt_password: var("MQTT_PASSWORD"),
            topic_prefix: var("MQTT_TOPIC_PREFIX"),
            clock_format: var("CLOCK_FORMAT").unwrap_or_else(|| "%H:%M".into()),
            date_format: var("DATE_FORMAT").unwrap_or_else(|| "%a %d %b".into()),
        })
    }
}
