use serde::{Deserialize, Serialize};

/// The parts of an OCI image config Graviton uses
/// (<https://github.com/opencontainers/image-spec/blob/main/config.md>).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub config: RuntimeConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RuntimeConfig {
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

impl RuntimeConfig {
    /// Entrypoint + (the caller's command, or the image's Cmd), like `docker run`.
    pub fn argv(&self, command: Option<&[String]>) -> Vec<String> {
        let mut argv = self.entrypoint.clone().unwrap_or_default();
        match command {
            Some(cmd) if !cmd.is_empty() => argv.extend(cmd.iter().cloned()),
            _ => argv.extend(self.cmd.clone().unwrap_or_default()),
        }
        argv
    }

    /// The image's environment with `extra` (`KEY=value`) overriding it.
    pub fn env(&self, extra: &[String]) -> Vec<String> {
        let mut env: Vec<String> = self.env.clone().unwrap_or_default();
        for pair in extra {
            let key = pair.split_once('=').map_or(pair.as_str(), |(k, _)| k);
            env.retain(|e| e.split_once('=').map_or(e.as_str(), |(k, _)| k) != key);
            env.push(pair.clone());
        }
        if !env.iter().any(|e| e.starts_with("PATH=")) {
            env.push("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
        }
        env
    }

    pub fn working_dir(&self) -> String {
        self.working_dir.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| "/".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_combines_like_docker() {
        let json = r#"{"architecture":"arm64","os":"linux","config":{"Env":["PATH=/usr/bin","LANG=C"],
            "Entrypoint":["docker-entrypoint.sh"],"Cmd":["node"],"WorkingDir":"/app","User":"node"}}"#;
        let cfg: ImageConfig = serde_json::from_str(json).unwrap();
        let rc = &cfg.config;
        assert_eq!(rc.argv(None), ["docker-entrypoint.sh", "node"]);
        assert_eq!(rc.argv(Some(&["bash".into()])), ["docker-entrypoint.sh", "bash"]);
        assert_eq!(rc.env(&["LANG=en_US.UTF-8".into()]), ["PATH=/usr/bin", "LANG=en_US.UTF-8"]);
        assert_eq!(rc.working_dir(), "/app");

        let bare: ImageConfig = serde_json::from_str(r#"{"config":{"Cmd":["/bin/sh"]}}"#).unwrap();
        assert_eq!(bare.config.argv(None), ["/bin/sh"]);
        assert!(bare.config.env(&[])[0].starts_with("PATH="));
        assert_eq!(bare.config.working_dir(), "/");
    }
}
