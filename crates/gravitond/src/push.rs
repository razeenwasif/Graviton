//! Push alerts to paired phones through Apple's push service (APNs).
//!
//! `grav push setup` hands gravitond the team's APNs key (a .p8) over the
//! local socket; it's kept in the login Keychain (service
//! `dev.graviton.apns`, account = the key id) and nowhere else. The key and
//! team ids, which aren't secret, go in `push.json`. Each alert is signed with
//! an ES256 token (reused for 50 minutes) and sent over HTTP/2 to the sandbox
//! host for debug builds of the app and the production host otherwise.
//!
//! Alerts go out for events worth interrupting someone for (a container that
//! failed, a build finished or failed). The text is the event's one line,
//! such as "web exited with code 1"; Apple relays it.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use graviton_core::paths;
use graviton_proto::{EngineEvent, PushInfo};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::remote::Remote;

/// How long an alert waits for the Keychain before giving up.
const KEYCHAIN_WAIT: Duration = Duration::from_secs(15);
const KEYCHAIN_SERVICE: &str = "dev.graviton.apns";
/// The iPhone app's bundle id.
const TOPIC: &str = "dev.graviton.ios";
const JWT_LIFETIME: Duration = Duration::from_secs(50 * 60);

#[derive(Clone, Serialize, Deserialize)]
struct Settings {
    key_id: String,
    team_id: String,
}

pub struct Push {
    remote: Arc<Remote>,
    rt: tokio::runtime::Handle,
    settings: Mutex<Option<Settings>>,
    jwt: Mutex<Option<(String, Instant)>>,
    http: reqwest::Client,
}

fn settings_path() -> PathBuf {
    paths::support_dir().join("push.json")
}

/// The DER inside a .p8 file (PEM, or bare base64).
fn p8_der(p8: &str) -> Result<Vec<u8>> {
    let body: String = p8.lines().filter(|l| !l.starts_with("-----")).collect::<Vec<_>>().join("");
    base64::engine::general_purpose::STANDARD.decode(body.trim()).context("that isn't a .p8 key")
}

/// An ES256 JWT for APNs, signed with the .p8 key.
fn apns_jwt(p8: &str, key_id: &str, team_id: &str, at: u64) -> Result<String> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &p8_der(p8)?, &rng).map_err(|e| anyhow!("the .p8 key can't be read: {e}"))?;
    let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let header = b64(json!({"alg": "ES256", "kid": key_id}).to_string().as_bytes());
    let claims = b64(json!({"iss": team_id, "iat": at}).to_string().as_bytes());
    let signing_input = format!("{header}.{claims}");
    let sig = key.sign(&rng, signing_input.as_bytes()).map_err(|_| anyhow!("signing failed"))?;
    Ok(format!("{signing_input}.{}", b64(sig.as_ref())))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Push {
    pub fn new(remote: Arc<Remote>, rt: tokio::runtime::Handle) -> Arc<Self> {
        let settings = fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let http = reqwest::Client::builder().http2_prior_knowledge().timeout(Duration::from_secs(15)).build().expect("HTTP client");
        Arc::new(Self { remote, rt, settings: Mutex::new(settings), jwt: Mutex::new(None), http })
    }

    pub fn info(&self) -> PushInfo {
        let s = self.settings.lock().unwrap().clone();
        PushInfo {
            configured: s.is_some(),
            key_id: s.as_ref().map(|s| s.key_id.clone()),
            team_id: s.map(|s| s.team_id),
            devices: self.remote.push_targets().len() as u32,
            note: None,
        }
    }

    /// Check the key, keep it in the Keychain, and remember its ids.
    pub fn setup(&self, key_id: &str, team_id: &str, p8: &str) -> Result<PushInfo> {
        let (key_id, team_id) = (key_id.trim().to_string(), team_id.trim().to_string());
        let valid = |s: &str| s.len() == 10 && s.bytes().all(|b| b.is_ascii_alphanumeric());
        if !valid(&key_id) || !valid(&team_id) {
            bail!("the APNs key id and team id are ten letters and digits each");
        }
        apns_jwt(p8, &key_id, &team_id, now())?;
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, &key_id, p8.as_bytes())
            .map_err(|e| anyhow!("can't save the key in the login Keychain: {e}"))?;
        let settings = Settings { key_id, team_id };
        fs::write(settings_path(), serde_json::to_vec_pretty(&settings)?)?;
        *self.settings.lock().unwrap() = Some(settings);
        *self.jwt.lock().unwrap() = None;
        Ok(self.info())
    }

    /// A signed token, reused until it's 50 minutes old.
    ///
    /// Reading the key can block: when gravitond's signature changes, the
    /// Keychain asks on the Mac's screen whether to let it, and waits for a
    /// click. That read once ran here on the engine's own threads with this
    /// lock held, so every alert after it queued on the lock and gravitond
    /// stopped answering anything. Now it runs on a blocking thread, with no
    /// lock held, and gives up after a while.
    async fn jwt(&self) -> Result<String> {
        let settings = self.settings.lock().unwrap().clone().ok_or_else(|| anyhow!("push alerts aren't set up: run `grav push setup`"))?;
        if let Some((jwt, _)) = self.jwt.lock().unwrap().as_ref().filter(|(_, made)| made.elapsed() < JWT_LIFETIME) {
            return Ok(jwt.clone());
        }
        let key_id = settings.key_id.clone();
        let read = tokio::task::spawn_blocking(move || security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, &key_id));
        let p8 = match tokio::time::timeout(KEYCHAIN_WAIT, read).await {
            Ok(Ok(Ok(p8))) => p8,
            Ok(Ok(Err(e))) => bail!("can't read the APNs key from the login Keychain ({e}); run `grav push setup` again"),
            Ok(Err(e)) => bail!("reading the APNs key: {e}"),
            Err(_) => bail!(
                "the login Keychain didn't hand over the APNs key within {} s; it may be asking on this Mac's screen whether gravitond may read it",
                KEYCHAIN_WAIT.as_secs()
            ),
        };
        let jwt = apns_jwt(std::str::from_utf8(&p8)?, &settings.key_id, &settings.team_id, now())?;
        *self.jwt.lock().unwrap() = Some((jwt.clone(), Instant::now()));
        Ok(jwt)
    }

    /// An engine event happened: alert the phones if it's worth it.
    pub fn notify(self: &Arc<Self>, event: &EngineEvent) {
        if !event.kind.is_alert() || self.settings.lock().unwrap().is_none() {
            return;
        }
        let push = Arc::clone(self);
        let (title, body, seq) = (self.remote.host_name().to_string(), event.message.clone(), event.seq);
        self.rt.spawn(async move {
            for (device, result) in push.send_all(&title, &body, Some(seq)).await {
                if let Err(err) = result {
                    eprintln!("push to {device}: {err:#}");
                }
            }
        });
    }

    /// Send one alert to every registered phone.
    pub async fn send_all(&self, title: &str, body: &str, seq: Option<u64>) -> Vec<(String, Result<()>)> {
        let mut results = Vec::new();
        for target in self.remote.push_targets() {
            let result = self.send(&target.token, target.sandbox, title, body, seq).await;
            if let Err(err) = &result {
                let text = format!("{err:#}");
                if text.contains("BadDeviceToken") || text.contains("Unregistered") {
                    self.remote.forget_push(&target.token);
                }
            }
            results.push((target.name, result));
        }
        results
    }

    async fn send(&self, token: &str, sandbox: bool, title: &str, body: &str, seq: Option<u64>) -> Result<()> {
        let jwt = self.jwt().await?;
        let host = if sandbox { "api.sandbox.push.apple.com" } else { "api.push.apple.com" };
        let payload = json!({
            "aps": {"alert": {"title": title, "body": body}, "sound": "default", "thread-id": "graviton"},
            "event": seq,
        });
        let response = self
            .http
            .post(format!("https://{host}/3/device/{token}"))
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-topic", TOPIC)
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .json(&payload)
            .send()
            .await
            .context("reaching APNs")?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let reason = response.text().await.unwrap_or_default();
        if status.as_u16() == 403 {
            // The token may have been signed with a revoked key or the wrong team.
            *self.jwt.lock().unwrap() = None;
        }
        bail!("APNs answered {status}: {reason}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_jwts_with_a_p8_key() {
        // A throwaway P-256 key in PKCS#8, as Apple's .p8 files are.
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref()));
        let jwt = apns_jwt(&pem, "ABCDE12345", "TEAM123456", 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        assert_eq!(String::from_utf8(header).unwrap(), r#"{"alg":"ES256","kid":"ABCDE12345"}"#);
        // ES256 signatures are 64 bytes (r || s).
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 64);
        assert!(apns_jwt("not a key", "A", "B", 0).is_err());
    }
}
