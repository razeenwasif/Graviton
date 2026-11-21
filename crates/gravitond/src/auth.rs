//! Saved OCI registry logins (`grav login` / `grav logout`).
//!
//! Usernames live in `registries.json` (mode `0600`) inside the Graviton
//! support directory; passwords/tokens live in the Mac's login Keychain under
//! service `dev.graviton.registry` (account = canonical registry name, e.g.
//! `docker.io`, `ghcr.io`).
//!
//! Public pulls for registries not in `registries.json` never touch the
//! Keychain. Keychain calls run on a helper thread with a timeout (matching
//! `push.rs`) so an interactive macOS Keychain prompt can never hang `gravitond`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::paths;
use graviton_image::{Credentials, Registry, normalize_registry};
use graviton_proto::RegistryLoginInfo;
use serde::{Deserialize, Serialize};

const KEYCHAIN_SERVICE: &str = "dev.graviton.registry";
const KEYCHAIN_WAIT: Duration = Duration::from_secs(15);
/// macOS Security.framework `errSecItemNotFound`.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LoginRecord {
    username: String,
    #[serde(default)]
    updated_at: u64,
}

trait SecretStore: Send + Sync {
    fn set(&self, account: &str, secret: &[u8]) -> Result<()>;
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>>;
    /// Returns `true` if a Keychain item existed and was deleted.
    fn delete(&self, account: &str) -> Result<bool>;
}

struct KeychainStore;

fn run_keychain<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("graviton-keychain".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .context("starting Keychain worker")?;
    match rx.recv_timeout(KEYCHAIN_WAIT) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => bail!(
            "the login Keychain didn't respond within {} s while {what}; it may be asking on this Mac's screen whether gravitond may access it",
            KEYCHAIN_WAIT.as_secs()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("Keychain worker exited unexpectedly while {what}"),
    }
}

impl SecretStore for KeychainStore {
    fn set(&self, account: &str, secret: &[u8]) -> Result<()> {
        let account = account.to_string();
        let secret = secret.to_vec();
        run_keychain("saving registry credentials", move || {
            security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, &account, &secret)
        })?
        .map_err(|e| anyhow!("can't save credentials in the login Keychain: {e}"))
    }

    fn get(&self, account: &str) -> Result<Option<Vec<u8>>> {
        let account = account.to_string();
        match run_keychain("reading registry credentials", move || {
            security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, &account)
        })? {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => bail!("can't read credentials from the login Keychain: {e}"),
        }
    }

    fn delete(&self, account: &str) -> Result<bool> {
        let account = account.to_string();
        match run_keychain("removing registry credentials", move || {
            security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, &account)
        })? {
            Ok(()) => Ok(true),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            Err(e) => bail!("can't remove credentials from the login Keychain: {e}"),
        }
    }
}

pub struct RegistryAuth {
    path: PathBuf,
    logins: Mutex<BTreeMap<String, LoginRecord>>,
    secrets: Box<dyn SecretStore>,
}

impl RegistryAuth {
    pub fn open() -> Self {
        Self::open_with(paths::support_dir().join("registries.json"), Box::new(KeychainStore))
    }

    fn open_with(path: PathBuf, secrets: Box<dyn SecretStore>) -> Self {
        let logins = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BTreeMap<String, LoginRecord>>(&bytes).ok())
            .unwrap_or_default();
        Self { path, logins: Mutex::new(logins), secrets }
    }

    /// Verify credentials against `registry`, save the password in the login
    /// Keychain, and remember the username on disk.
    pub fn login(&self, registry: &str, username: &str, password: &str) -> Result<(String, String)> {
        let username = username.trim().to_string();
        if username.is_empty() {
            bail!("username can't be empty");
        }
        if password.is_empty() {
            bail!("password can't be empty");
        }
        let creds = Credentials { username: username.clone(), password: password.to_string() };
        let canonical = Registry::new().verify_login(registry, &creds)?;
        self.save_verified(&canonical, &username, password)?;
        Ok((canonical, username))
    }

    fn save_verified(&self, canonical: &str, username: &str, password: &str) -> Result<()> {
        self.secrets.set(canonical, password.as_bytes())?;
        let mut logins = self.logins.lock().unwrap();
        logins.insert(
            canonical.to_string(),
            LoginRecord {
                username: username.to_string(),
                updated_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
            },
        );
        save_map(&self.path, &logins)
    }

    /// Forget saved credentials for `registry`.
    pub fn logout(&self, registry: &str) -> Result<String> {
        let canonical = normalize_registry(registry);
        let removed_secret = self.secrets.delete(&canonical)?;
        let mut logins = self.logins.lock().unwrap();
        let removed_meta = logins.remove(&canonical).is_some();
        if removed_meta {
            save_map(&self.path, &logins)?;
        }
        if !removed_meta && !removed_secret {
            bail!("not logged in to {canonical}");
        }
        Ok(canonical)
    }

    /// Saved logins (usernames only, sorted by registry name).
    pub fn list(&self) -> Vec<RegistryLoginInfo> {
        self.logins
            .lock()
            .unwrap()
            .iter()
            .map(|(registry, rec)| RegistryLoginInfo { registry: registry.clone(), username: rec.username.clone() })
            .collect()
    }

    /// Look up saved credentials for `registry`. Returns `Ok(None)` without
    /// touching the Keychain when no login is saved for that registry.
    pub fn credentials_for(&self, registry: &str) -> Result<Option<Credentials>> {
        let canonical = normalize_registry(registry);
        let Some(username) = self.logins.lock().unwrap().get(&canonical).map(|r| r.username.clone()) else {
            return Ok(None);
        };
        let Some(secret) = self.secrets.get(&canonical)? else {
            bail!("saved credentials for {canonical} are missing from the login Keychain; run `grav login {canonical}` again");
        };
        let password = String::from_utf8(secret)
            .map_err(|_| anyhow!("saved credentials for {canonical} in the login Keychain aren't valid UTF-8; run `grav login {canonical}` again"))?;
        Ok(Some(Credentials { username, password }))
    }
}

fn save_map(path: &std::path::Path, map: &BTreeMap<String, LoginRecord>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(map)?;
    fs::write(&tmp, &data).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    #[derive(Default)]
    struct MemorySecrets(Arc<Mutex<HashMap<String, Vec<u8>>>>);

    impl SecretStore for MemorySecrets {
        fn set(&self, account: &str, secret: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(account.to_string(), secret.to_vec());
            Ok(())
        }
        fn get(&self, account: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn delete(&self, account: &str) -> Result<bool> {
            Ok(self.0.lock().unwrap().remove(account).is_some())
        }
    }

    #[test]
    fn saves_lists_looks_up_and_logs_out() {
        let dir = std::env::temp_dir().join(format!("graviton-auth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("registries.json");
        let backing = Arc::new(Mutex::new(HashMap::new()));

        let auth = RegistryAuth::open_with(path.clone(), Box::new(MemorySecrets(Arc::clone(&backing))));
        assert!(auth.credentials_for("docker.io").unwrap().is_none());
        assert!(auth.list().is_empty());

        auth.save_verified("docker.io", "alice", "tok_123").unwrap();
        auth.save_verified("ghcr.io", "octocat", "ghp_456").unwrap();

        // File on disk has usernames only, never passwords, and 0600 permissions.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("alice") && raw.contains("octocat"));
        assert!(!raw.contains("tok_123") && !raw.contains("ghp_456"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Re-opening from disk restores logins and resolves Docker Hub aliases.
        let reloaded = RegistryAuth::open_with(path.clone(), Box::new(MemorySecrets(Arc::clone(&backing))));
        let list = reloaded.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], RegistryLoginInfo { registry: "docker.io".into(), username: "alice".into() });
        assert_eq!(list[1], RegistryLoginInfo { registry: "ghcr.io".into(), username: "octocat".into() });

        let creds = reloaded.credentials_for("index.docker.io").unwrap().unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "tok_123");

        assert_eq!(reloaded.logout("https://GHCR.IO/").unwrap(), "ghcr.io");
        assert!(reloaded.credentials_for("ghcr.io").unwrap().is_none());
        assert!(reloaded.logout("ghcr.io").is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
