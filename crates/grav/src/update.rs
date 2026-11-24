//! Self-update check for the `grav` CLI (`grav update`), matching `UpdateChecker.swift`.

use std::cmp::Ordering;
use std::fmt;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

pub const DEFAULT_RELEASE_URL: &str = "https://api.github.com/repos/razeenwasif/Graviton/releases/latest";
pub const HOMEBREW_UPGRADE_CMD: &str = "brew upgrade --cask graviton";

/// Minimal SemVer 2.0.0 parser and comparator (ignores `+build` metadata in precedence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Vec<String>,
}

impl SemVer {
    pub fn parse(raw: &str) -> Option<Self> {
        let mut s = raw.trim();
        if let Some(rest) = s.strip_prefix('v').or_else(|| s.strip_prefix('V')) {
            s = rest;
        }
        if s.is_empty() {
            return None;
        }
        if let Some((before_build, _)) = s.split_once('+') {
            s = before_build;
        }
        let (core_str, prerelease) = match s.split_once('-') {
            Some((core, pre)) => {
                if pre.is_empty() {
                    return None;
                }
                let tokens: Vec<String> = pre.split('.').map(String::from).collect();
                if tokens.iter().any(|t| t.is_empty()) {
                    return None;
                }
                (core, tokens)
            }
            None => (s, Vec::new()),
        };

        let parts: Vec<&str> = core_str.split('.').collect();
        if parts.len() != 2 && parts.len() != 3 {
            return None;
        }
        let major: u64 = parts[0].parse().ok()?;
        let minor: u64 = parts[1].parse().ok()?;
        let patch: u64 = if parts.len() == 3 { parts[2].parse().ok()? } else { 0 };
        Some(Self { major, minor, patch, prerelease })
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prerelease.is_empty() {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        } else {
            write!(f, "{}.{}.{}-{}", self.major, self.minor, self.patch, self.prerelease.join("."))
        }
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.prerelease.iter().zip(&other.prerelease) {
                        if a == b {
                            continue;
                        }
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(an), Ok(bn)) => an.cmp(&bn),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        return ord;
                    }
                    self.prerelease.len().cmp(&other.prerelease.len())
                }
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GitHubReleasePayload {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    prerelease: Option<bool>,
    #[serde(default)]
    assets: Option<Vec<GitHubAsset>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateOutcome {
    UpToDate {
        current: String,
        latest: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        release_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Available {
        current: String,
        latest: String,
        tag: String,
        title: String,
        release_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dmg_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        zip_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        published_at: Option<String>,
        notes: String,
        brew_upgrade: String,
    },
    Unavailable {
        current: String,
        reason: String,
    },
}

impl UpdateOutcome {
    pub fn check_exit_code(&self) -> i32 {
        match self {
            Self::UpToDate { .. } => 0,
            Self::Available { .. } => 1,
            Self::Unavailable { .. } => 2,
        }
    }
}

pub fn evaluate_release_response(current_raw: &str, status: u16, body: &str) -> UpdateOutcome {
    let current_ver = SemVer::parse(current_raw).unwrap_or(SemVer {
        major: 0,
        minor: 1,
        patch: 0,
        prerelease: Vec::new(),
    });
    let current = current_ver.to_string();

    match status {
        200 => {
            let payload: GitHubReleasePayload = match serde_json::from_str(body) {
                Ok(p) => p,
                Err(err) => {
                    return UpdateOutcome::Unavailable {
                        current,
                        reason: format!("Could not parse release feed ({err})."),
                    };
                }
            };
            if payload.draft == Some(true) || payload.prerelease == Some(true) {
                return UpdateOutcome::UpToDate {
                    current: current.clone(),
                    latest: current,
                    release_url: Some(payload.html_url),
                    note: Some(format!("Ignored draft/prerelease tag {}.", payload.tag_name)),
                };
            }
            let Some(latest_ver) = SemVer::parse(&payload.tag_name) else {
                return UpdateOutcome::Unavailable {
                    current,
                    reason: format!("Release tag `{}` is not valid SemVer.", payload.tag_name),
                };
            };
            let latest = latest_ver.to_string();
            if latest_ver > current_ver {
                let assets = payload.assets.unwrap_or_default();
                let dmg_url = assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with(".dmg"))
                    .map(|a| a.browser_download_url.clone());
                let zip_url = assets
                    .iter()
                    .find(|a| a.name.to_ascii_lowercase().ends_with(".zip"))
                    .map(|a| a.browser_download_url.clone());
                let title = payload
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| payload.tag_name.clone());
                UpdateOutcome::Available {
                    current,
                    latest,
                    tag: payload.tag_name,
                    title,
                    release_url: payload.html_url,
                    dmg_url,
                    zip_url,
                    published_at: payload.published_at,
                    notes: payload.body.unwrap_or_default().trim().to_string(),
                    brew_upgrade: HOMEBREW_UPGRADE_CMD.into(),
                }
            } else {
                UpdateOutcome::UpToDate {
                    current,
                    latest,
                    release_url: Some(payload.html_url),
                    note: None,
                }
            }
        }
        404 => UpdateOutcome::Unavailable {
            current,
            reason: "No public release feed found yet (repository may be private or have no published releases).".into(),
        },
        403 | 429 => UpdateOutcome::Unavailable {
            current,
            reason: "GitHub API rate limit reached; try again later.".into(),
        },
        code => UpdateOutcome::Unavailable {
            current,
            reason: format!("Update check returned HTTP {code}."),
        },
    }
}

fn feed_url() -> String {
    std::env::var("GRAVITON_UPDATE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_URL.to_string())
}

fn auth_token() -> Option<String> {
    std::env::var("GRAVITON_UPDATE_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn fetch_latest_release(url: &str, current_version: &str, token: Option<&str>) -> UpdateOutcome {
    let curl = if Path::new("/usr/bin/curl").exists() {
        "/usr/bin/curl"
    } else {
        "curl"
    };
    let mut cmd = Command::new(curl);
    cmd.args([
        "-sS",
        "-L",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "X-GitHub-Api-Version: 2022-11-28",
        "-H",
        &format!("User-Agent: grav/{current_version}"),
        "--connect-timeout",
        "8",
        "--max-time",
        "15",
        "-w",
        "\n%{http_code}",
    ]);
    if let Some(tok) = token {
        cmd.arg("-H").arg(format!("Authorization: Bearer {tok}"));
    }
    cmd.arg(url);

    let out = match cmd.output() {
        Ok(o) => o,
        Err(err) => {
            return UpdateOutcome::Unavailable {
                current: current_version.to_string(),
                reason: format!("Couldn't run {curl} to check GitHub Releases ({err})."),
            };
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        return UpdateOutcome::Unavailable {
            current: current_version.to_string(),
            reason: if msg.is_empty() {
                "Couldn't reach GitHub Releases.".into()
            } else {
                format!("Couldn't reach GitHub Releases: {msg}")
            },
        };
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let Some((body, code_str)) = text.rsplit_once('\n') else {
        return UpdateOutcome::Unavailable {
            current: current_version.to_string(),
            reason: "Unexpected response while checking GitHub Releases.".into(),
        };
    };
    let Ok(status) = code_str.trim().parse::<u16>() else {
        return UpdateOutcome::Unavailable {
            current: current_version.to_string(),
            reason: format!("Invalid HTTP status `{}` from release feed.", code_str.trim()),
        };
    };
    evaluate_release_response(current_version, status, body)
}

pub fn run_update(check: bool, json_output: bool) -> i32 {
    let current = env!("CARGO_PKG_VERSION");
    let url = feed_url();
    let token = auth_token();
    let outcome = fetch_latest_release(&url, current, token.as_deref());

    if json_output {
        println!("{}", serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "{}".into()));
    } else {
        match &outcome {
            UpdateOutcome::UpToDate { current, latest, note, .. } => {
                if let Some(n) = note {
                    println!("Graviton {current} is up to date ({n}).");
                } else {
                    println!("Graviton {current} is up to date (latest: {latest}).");
                }
            }
            UpdateOutcome::Available {
                current,
                latest,
                title,
                release_url,
                dmg_url,
                notes,
                brew_upgrade,
                ..
            } => {
                println!("Graviton {latest} is available (installed: {current}) — {title}");
                println!("Release:  {release_url}");
                if let Some(dmg) = dmg_url {
                    println!("DMG:      {dmg}");
                }
                println!("Homebrew: {brew_upgrade}");
                if !notes.is_empty() {
                    println!("\nNotes:\n{notes}");
                }
            }
            UpdateOutcome::Unavailable { current, reason } => {
                eprintln!("Graviton {current}: update check unavailable — {reason}");
            }
        }
    }

    if check {
        outcome.check_exit_code()
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing_and_ordering() {
        let v010 = SemVer::parse("v0.1.0").unwrap();
        let v020 = SemVer::parse("0.2.0").unwrap();
        let v02_short = SemVer::parse("V0.2").unwrap();
        assert!(v010 < v020);
        assert_eq!(v020, v02_short);
        assert_eq!(SemVer::parse("0.2.0+build.42").unwrap(), v020);

        let alpha1 = SemVer::parse("1.0.0-alpha.1").unwrap();
        let alpha_beta = SemVer::parse("1.0.0-alpha.beta").unwrap();
        let num = SemVer::parse("1.0.0-1").unwrap();
        let stable = SemVer::parse("1.0.0").unwrap();
        assert!(num < alpha1);
        assert!(alpha1 < alpha_beta);
        assert!(alpha_beta < stable);

        assert!(SemVer::parse("").is_none());
        assert!(SemVer::parse("1").is_none());
        assert!(SemVer::parse("0.1.0-").is_none());
    }

    #[test]
    fn evaluate_release_response_handles_all_cases() {
        let same = r#"{"tag_name":"v0.1.0","name":"v0.1.0","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.1.0","draft":false,"prerelease":false,"assets":[]}"#;
        let out_same = evaluate_release_response("0.1.0", 200, same);
        assert!(matches!(out_same, UpdateOutcome::UpToDate { ref current, ref latest, .. } if current == "0.1.0" && latest == "0.1.0"));
        assert_eq!(out_same.check_exit_code(), 0);

        let pre = r#"{"tag_name":"v0.9.0-rc.1","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.9.0-rc.1","draft":false,"prerelease":true,"assets":[]}"#;
        let out_pre = evaluate_release_response("0.1.0", 200, pre);
        assert!(matches!(out_pre, UpdateOutcome::UpToDate { note: Some(_), .. }));
        assert_eq!(out_pre.check_exit_code(), 0);

        let newer = r#"{"tag_name":"v0.2.0","name":"Graviton v0.2.0","body":"User networks + ONBUILD","html_url":"https://github.com/razeenwasif/Graviton/releases/tag/v0.2.0","published_at":"2026-11-24T05:00:00Z","draft":false,"prerelease":false,"assets":[{"name":"Graviton-0.2.0.zip","browser_download_url":"https://example.com/Graviton-0.2.0.zip"},{"name":"Graviton-0.2.0.dmg","browser_download_url":"https://example.com/Graviton-0.2.0.dmg"}]}"#;
        let out_new = evaluate_release_response("0.1.0", 200, newer);
        match &out_new {
            UpdateOutcome::Available { current, latest, dmg_url, zip_url, brew_upgrade, .. } => {
                assert_eq!(current, "0.1.0");
                assert_eq!(latest, "0.2.0");
                assert_eq!(dmg_url.as_deref(), Some("https://example.com/Graviton-0.2.0.dmg"));
                assert_eq!(zip_url.as_deref(), Some("https://example.com/Graviton-0.2.0.zip"));
                assert_eq!(brew_upgrade, HOMEBREW_UPGRADE_CMD);
            }
            other => panic!("expected Available, got {other:?}"),
        }
        assert_eq!(out_new.check_exit_code(), 1);
        let json = serde_json::to_string(&out_new).unwrap();
        assert!(json.contains(r#""status":"available""#));
        assert!(json.contains(r#""brew_upgrade":"brew upgrade --cask graviton""#));

        let not_found = evaluate_release_response("0.1.0", 404, r#"{"message":"Not Found"}"#);
        assert!(matches!(not_found, UpdateOutcome::Unavailable { .. }));
        assert_eq!(not_found.check_exit_code(), 2);

        let limited = evaluate_release_response("0.1.0", 429, "rate limit");
        assert!(matches!(limited, UpdateOutcome::Unavailable { ref reason, .. } if reason.contains("rate limit")));
        assert_eq!(limited.check_exit_code(), 2);

        let bad_json = evaluate_release_response("0.1.0", 200, "not json");
        assert!(matches!(bad_json, UpdateOutcome::Unavailable { .. }));
    }
}
