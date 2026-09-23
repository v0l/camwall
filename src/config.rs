use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    Grid,
    Feature,
}

pub struct Config {
    pub frigate_url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub insecure: bool,
    pub cameras: Option<Vec<String>>,
    pub interval: Duration,
    pub layout: Layout,
    pub screensaver: Option<Duration>,
    pub screen_off_command: Option<String>,
    pub screen_on_command: Option<String>,
    pub clock_format: String,
    pub date_format: String,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn seconds(key: &str) -> Result<Option<f32>, String> {
    var(key)
        .map(|v| {
            v.parse::<f32>()
                .map_err(|_| format!("{key}: expected seconds, got {v:?}"))
        })
        .transpose()
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let frigate_url = var("FRIGATE_URL")
            .ok_or("FRIGATE_URL is required, e.g. https://frigate.local:8971")?
            .trim_end_matches('/')
            .to_string();
        if !frigate_url.starts_with("http://") && !frigate_url.starts_with("https://") {
            return Err(format!(
                "FRIGATE_URL must start with http:// or https://, got {frigate_url:?}"
            ));
        }
        let layout = match var("LAYOUT").as_deref() {
            None | Some("grid") => Layout::Grid,
            Some("feature") => Layout::Feature,
            Some(other) => return Err(format!("LAYOUT: expected grid or feature, got {other:?}")),
        };
        Ok(Self {
            frigate_url,
            user: var("FRIGATE_USER"),
            password: var("FRIGATE_PASSWORD"),
            insecure: matches!(
                var("FRIGATE_INSECURE").as_deref(),
                Some("1" | "true" | "yes")
            ),
            cameras: var("CAMERAS").map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }),
            interval: Duration::from_secs_f32(seconds("INTERVAL")?.unwrap_or(0.5).max(0.05)),
            layout,
            screensaver: seconds("SCREENSAVER")?
                .filter(|s| *s > 0.0)
                .map(Duration::from_secs_f32),
            screen_off_command: var("SCREEN_OFF_COMMAND"),
            screen_on_command: var("SCREEN_ON_COMMAND"),
            clock_format: var("CLOCK_FORMAT").unwrap_or_else(|| "%H:%M".into()),
            date_format: var("DATE_FORMAT").unwrap_or_else(|| "%a %d %b".into()),
        })
    }
}
