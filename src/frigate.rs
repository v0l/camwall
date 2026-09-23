use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{self, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, WebSocket};

use crate::config::Config;

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

pub struct Frigate {
    agent: ureq::Agent,
    tls: Arc<ClientConfig>,
    url: String,
    credentials: Option<(String, String)>,
    cookie: Mutex<Option<String>>,
}

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config(insecure: bool) -> Arc<ClientConfig> {
    let provider = Arc::new(crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions");
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth()
    } else {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Arc::new(config)
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
            tls: tls_config(cfg.insecure),
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

    fn session(&self) -> Result<Option<String>, String> {
        let Some((user, password)) = &self.credentials else {
            return Ok(None);
        };
        let mut guard = self.cookie.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.login(user, password)?);
        }
        Ok(guard.clone())
    }

    fn unauthorized(&self) -> Result<(), String> {
        if self.credentials.is_none() {
            return Err("unauthorized, set FRIGATE_USER and FRIGATE_PASSWORD".into());
        }
        *self.cookie.lock().unwrap() = None;
        Ok(())
    }

    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Vec<u8>, String> {
        for _ in 0..2 {
            let mut req = self.agent.get(format!("{}{}", self.url, path));
            for (k, v) in query {
                req = req.query(*k, v);
            }
            if let Some(cookie) = self.session()? {
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
                401 => self.unauthorized()?,
                s => return Err(format!("http {s}")),
            }
        }
        Err("unauthorized".into())
    }

    fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value, String> {
        serde_json::from_slice(&self.get(path, query)?).map_err(|e| e.to_string())
    }

    pub fn snapshot(&self, camera: &str, height: u32) -> Result<Vec<u8>, String> {
        self.get(
            &format!("/api/{camera}/latest.jpg"),
            &[("height", height.to_string()), ("quality", "85".into())],
        )
    }

    pub fn event_image(&self, id: &str, snapshot: bool, height: u32) -> Result<Vec<u8>, String> {
        if !snapshot {
            return self.get(&format!("/api/events/{id}/thumbnail.jpg"), &[]);
        }
        self.get(
            &format!("/api/events/{id}/snapshot.jpg"),
            &[("height", height.to_string()), ("quality", "85".into())],
        )
    }

    pub fn config(&self) -> Result<Value, String> {
        self.get_json("/api/config", &[])
    }

    pub fn latest_event(&self, cameras: &[String]) -> Result<Option<Value>, String> {
        let events = self.get_json(
            "/api/events",
            &[("limit", "1".into()), ("cameras", cameras.join(","))],
        )?;
        Ok(events.as_array().and_then(|a| a.first()).cloned())
    }

    pub fn connect(&self) -> Result<Socket, String> {
        let ws_url = format!(
            "ws{}/ws",
            self.url.strip_prefix("http").unwrap_or(&self.url)
        );
        for _ in 0..2 {
            let mut req = ws_url
                .as_str()
                .into_client_request()
                .map_err(|e| e.to_string())?;
            if let Some(cookie) = self.session()? {
                let value = cookie
                    .parse()
                    .map_err(|_| "bad session cookie".to_string())?;
                req.headers_mut().insert("Cookie", value);
            }
            let uri = req.uri().clone();
            let secure = uri.scheme_str() == Some("wss");
            let host = uri.host().ok_or("FRIGATE_URL has no host")?;
            let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
            let addr = (host.trim_matches(['[', ']']), port)
                .to_socket_addrs()
                .map_err(|e| e.to_string())?
                .next()
                .ok_or("FRIGATE_URL host did not resolve")?;
            let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
                .map_err(|e| e.to_string())?;
            tcp.set_read_timeout(Some(Duration::from_secs(90)))
                .map_err(|e| e.to_string())?;
            let connector = if secure {
                Connector::Rustls(self.tls.clone())
            } else {
                Connector::Plain
            };
            match tungstenite::client_tls_with_config(req, tcp, None, Some(connector)) {
                Ok((socket, _)) => return Ok(socket),
                Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp)))
                    if resp.status() == 401 =>
                {
                    self.unauthorized()?
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Err("unauthorized".into())
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
