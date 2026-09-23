use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;

pub struct Frigate {
    agent: ureq::Agent,
    url: String,
    credentials: Option<(String, String)>,
    cookie: Mutex<Option<String>>,
}

pub struct MqttSettings {
    pub host: String,
    pub port: u16,
    pub topic_prefix: String,
}

fn json_str(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

impl Frigate {
    pub fn new(cfg: &Config) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .disable_verification(cfg.insecure)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build()
            .into();
        let credentials = cfg
            .user
            .clone()
            .map(|u| (u, cfg.password.clone().unwrap_or_default()));
        Self {
            agent,
            url: cfg.frigate_url.clone(),
            credentials,
            cookie: Mutex::new(None),
        }
    }

    fn login(&self, user: &str, password: &str) -> Result<String, String> {
        let body = format!(
            "{{\"user\":{},\"password\":{}}}",
            json_str(user),
            json_str(password)
        );
        let resp = self
            .agent
            .post(format!("{}/api/login", self.url))
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| e.to_string())?;
        if resp.status() != 200 {
            return Err(format!("login failed: http {}", resp.status().as_u16()));
        }
        let cookie = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .collect::<Vec<_>>()
            .join("; ");
        if cookie.is_empty() {
            return Err("login returned no session cookie".into());
        }
        Ok(cookie)
    }

    fn cookie(&self) -> Result<Option<String>, String> {
        let Some((user, password)) = &self.credentials else {
            return Ok(None);
        };
        let mut guard = self.cookie.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.login(user, password)?);
        }
        Ok(guard.clone())
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<u8>, String> {
        for _ in 0..2 {
            let mut req = self.agent.get(format!("{}{}", self.url, path));
            for (k, v) in query {
                req = req.query(*k, v);
            }
            if let Some(cookie) = self.cookie()? {
                req = req.header("Cookie", &cookie);
            }
            let mut resp = req.call().map_err(|e| e.to_string())?;
            match resp.status().as_u16() {
                200 => {
                    return resp
                        .body_mut()
                        .with_config()
                        .limit(32 << 20)
                        .read_to_vec()
                        .map_err(|e| e.to_string())
                }
                401 if self.credentials.is_some() => *self.cookie.lock().unwrap() = None,
                401 => return Err("unauthorized, set FRIGATE_USER and FRIGATE_PASSWORD".into()),
                s => return Err(format!("http {s}")),
            }
        }
        Err("unauthorized".into())
    }

    pub fn snapshot(&self, camera: &str, height: u32) -> Result<Vec<u8>, String> {
        self.get(
            &format!("/api/{camera}/latest.jpg"),
            &[("height", height.to_string()), ("quality", "85".into())],
        )
    }

    pub fn config(&self) -> Result<Value, String> {
        let body = self.get("/api/config", &[])?;
        serde_json::from_slice(&body).map_err(|e| e.to_string())
    }
}

pub fn dashboard_cameras(config: &Value) -> Vec<String> {
    let Some(cameras) = config["cameras"].as_object() else {
        return Vec::new();
    };
    let mut list: Vec<(i64, &String)> = cameras
        .iter()
        .filter(|(name, _)| !name.starts_with('_'))
        .filter(|(_, c)| c["enabled"].as_bool() != Some(false))
        .filter(|(_, c)| c["ui"]["dashboard"].as_bool() != Some(false))
        .map(|(name, c)| (c["ui"]["order"].as_i64().unwrap_or(0), name))
        .collect();
    list.sort();
    list.into_iter().map(|(_, n)| n.clone()).collect()
}

pub fn mqtt_settings(config: &Value) -> Option<MqttSettings> {
    let mqtt = &config["mqtt"];
    if mqtt["enabled"].as_bool() == Some(false) {
        return None;
    }
    Some(MqttSettings {
        host: mqtt["host"].as_str()?.to_string(),
        port: mqtt["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(1883),
        topic_prefix: mqtt["topic_prefix"].as_str().unwrap_or("frigate").to_string(),
    })
}
