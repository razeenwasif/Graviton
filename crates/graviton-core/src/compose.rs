//! Docker Compose (`compose.yaml` / `docker-compose.yml`) parser, `.env`
//! interpolator, multi-file merger, and dependency resolver.
//!
//! Covers the practical Compose Specification:
//! - Default file discovery (`compose.yaml`, `compose.yml`, `docker-compose.yml`,
//!   `docker-compose.yaml`) with automatic `*.override.*` merging or explicit `-f`
//! - `.env` and host-environment `${VAR}` interpolation (`$$`, `${VAR:-def}`,
//!   `${VAR-def}`, `${VAR:?err}`, `${VAR:+alt}`)
//! - YAML 1.2 anchors, aliases, and `<<:` merge keys (plus `x-*` extensions)
//! - `image`, `build` (short string or `{context, dockerfile, args, target, platform}`),
//!   `container_name`, `hostname`, `platform`, `command`, `entrypoint`, `working_dir`,
//!   `user`, `environment` (list/map), `env_file`, `ports` (short/long), `volumes`
//!   (short/long, bind + named with `external`/`name`), `depends_on` (list/map with
//!   `service_started` / `service_completed_successfully`), `restart`, `labels`,
//!   `cpus` / `mem_limit` / `deploy.resources.limits`, `networks.<n>.aliases`, and
//!   `profiles`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use graviton_proto::{Healthcheck, RestartPolicy, parse_duration_secs};
use serde::Serialize;
use thiserror::Error;
use yaml_rust2::{Yaml, YamlLoader, yaml::Hash as YamlHash};

const DEFAULT_FILES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"];
const OVERRIDE_FILES: [&str; 4] = [
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yml",
    "docker-compose.override.yaml",
];

#[derive(Debug, Error)]
pub enum ComposeError {
    #[error("no compose.yaml or docker-compose.yml found in {0}")]
    NoComposeFile(String),
    #[error("reading {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("invalid YAML in {path}: {message}")]
    Yaml { path: String, message: String },
    #[error("interpolation error: {0}")]
    Interpolation(String),
    #[error("{0}")]
    Spec(String),
    #[error("unknown service `{0}`")]
    UnknownService(String),
    #[error("dependency cycle involving service `{0}`")]
    Cycle(String),
}

pub type Result<T, E = ComposeError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComposeProject {
    pub name: String,
    pub project_dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub services: BTreeMap<String, ComposeService>,
    pub volumes: BTreeMap<String, ComposeVolumeDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComposeVolumeDef {
    /// Actual named volume name in Graviton (`<project>_<key>` unless `external` or custom `name`).
    pub volume_name: String,
    pub external: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComposeBuild {
    pub context: PathBuf,
    pub dockerfile: Option<String>,
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependsCondition {
    ServiceStarted,
    ServiceCompletedSuccessfully,
    ServiceHealthy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComposeService {
    pub name: String,
    pub container_name: String,
    pub image: String,
    pub build: Option<ComposeBuild>,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub environment: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub volumes: Vec<String>,
    pub depends_on: BTreeMap<String, DependsCondition>,
    pub restart: Option<RestartPolicy>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub hostname: Option<String>,
    pub platform: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub labels: BTreeMap<String, String>,
    pub dns_aliases: Vec<String>,
    pub profiles: Vec<String>,
    pub healthcheck: Option<Healthcheck>,
}

/// Options for loading a Compose project from disk.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub files: Vec<PathBuf>,
    pub project_name: Option<String>,
    pub env_file: Option<PathBuf>,
}

impl ComposeProject {
    /// Discover, interpolate, parse, and merge Compose file(s) from `cwd`.
    pub fn load(cwd: &Path, opts: &LoadOptions) -> Result<Self> {
        let host_env: BTreeMap<String, String> = std::env::vars().collect();
        Self::load_with_env(cwd, opts, &host_env)
    }

    /// Same as [`Self::load`], taking an explicit host environment map (for deterministic tests).
    pub fn load_with_env(cwd: &Path, opts: &LoadOptions, host_env: &BTreeMap<String, String>) -> Result<Self> {
        let files = resolve_files(cwd, &opts.files)?;
        let project_dir = files[0]
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf());

        // Load `.env` (or explicit `--env-file`), then overlay host process env (host wins).
        let mut vars = BTreeMap::new();
        let dot_env = match &opts.env_file {
            Some(p) => {
                let path = if p.is_absolute() { p.clone() } else { cwd.join(p) };
                Some((path, true))
            }
            None => {
                let default = project_dir.join(".env");
                default.is_file().then_some((default, false))
            }
        };
        if let Some((path, required)) = dot_env {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    for (k, v) in parse_dotenv(&text) {
                        vars.insert(k, v);
                    }
                }
                Err(source) if required => return Err(ComposeError::Io { path: path.display().to_string(), source }),
                Err(_) => {}
            }
        }
        for (k, v) in host_env {
            vars.insert(k.clone(), v.clone());
        }

        let mut merged_root = YamlHash::new();
        for file in &files {
            let raw = fs::read_to_string(file).map_err(|source| ComposeError::Io { path: file.display().to_string(), source })?;
            let expanded = interpolate(&raw, &vars)?;
            let docs = YamlLoader::load_from_str(&expanded).map_err(|e| ComposeError::Yaml {
                path: file.display().to_string(),
                message: e.to_string(),
            })?;
            let Some(doc) = docs.into_iter().next() else { continue };
            let resolved = resolve_yaml_merges(doc);
            let Yaml::Hash(map) = resolved else {
                return Err(ComposeError::Spec(format!("{}: top-level document must be a mapping", file.display())));
            };
            merge_root_maps(&mut merged_root, map);
        }

        let file_name = yaml_get_str(&merged_root, "name");
        let raw_project = opts
            .project_name
            .clone()
            .or_else(|| host_env.get("COMPOSE_PROJECT_NAME").cloned().filter(|s| !s.is_empty()))
            .or(file_name)
            .unwrap_or_else(|| {
                project_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "graviton".into())
            });
        let project_name = sanitize_project_name(&raw_project);

        // Parse top-level named volumes.
        let mut volumes = BTreeMap::new();
        if let Some(Yaml::Hash(vol_map)) = yaml_get(&merged_root, "volumes") {
            for (k, v) in vol_map {
                let Some(key) = k.as_str() else { continue };
                let (external, custom_name) = match v {
                    Yaml::Hash(h) => {
                        let ext = match yaml_get(h, "external") {
                            Some(Yaml::Boolean(b)) => *b,
                            Some(Yaml::Hash(_)) => true,
                            _ => false,
                        };
                        let custom = yaml_get_str(h, "name").or_else(|| {
                            yaml_get(h, "external")
                                .and_then(Yaml::as_hash)
                                .and_then(|eh| yaml_get_str(eh, "name"))
                        });
                        (ext, custom)
                    }
                    _ => (false, None),
                };
                let volume_name = match custom_name {
                    Some(n) => n,
                    None if external => key.to_string(),
                    None => format!("{project_name}_{key}"),
                };
                volumes.insert(key.to_string(), ComposeVolumeDef { volume_name, external });
            }
        }

        // Parse services.
        let Some(Yaml::Hash(svc_map)) = yaml_get(&merged_root, "services") else {
            return Err(ComposeError::Spec("compose file has no `services:` mapping".into()));
        };
        if svc_map.is_empty() {
            return Err(ComposeError::Spec("compose file defines no services".into()));
        }

        let mut services = BTreeMap::new();
        for (k, v) in svc_map {
            let Some(svc_name) = k.as_str() else { continue };
            let Yaml::Hash(h) = v else {
                return Err(ComposeError::Spec(format!("service `{svc_name}` must be a mapping")));
            };
            let svc = parse_service(&project_name, svc_name, h, &project_dir, &volumes, &vars)?;
            services.insert(svc_name.to_string(), svc);
        }

        // Validate depends_on targets exist.
        for svc in services.values() {
            for dep in svc.depends_on.keys() {
                if !services.contains_key(dep) {
                    return Err(ComposeError::Spec(format!(
                        "service `{}` depends_on unknown service `{dep}`",
                        svc.name
                    )));
                }
            }
        }

        Ok(Self { name: project_name, project_dir, files, services, volumes })
    }

    /// Return services in topological startup order (dependencies first).
    ///
    /// - If `selected` is non-empty, includes those services plus all transitive `depends_on`.
    /// - Otherwise includes every service whose `profiles` is empty OR intersects `profiles`.
    pub fn startup_order(&self, selected: &[String], profiles: &[String]) -> Result<Vec<&ComposeService>> {
        let mut needed: BTreeSet<String> = BTreeSet::new();
        if selected.is_empty() {
            let active_profiles: BTreeSet<&str> = profiles.iter().map(String::as_str).collect();
            for (name, svc) in &self.services {
                if svc.profiles.is_empty() || svc.profiles.iter().any(|p| active_profiles.contains(p.as_str())) {
                    needed.insert(name.clone());
                }
            }
        } else {
            for s in selected {
                if !self.services.contains_key(s) {
                    return Err(ComposeError::UnknownService(s.clone()));
                }
                needed.insert(s.clone());
            }
        }

        // Transitively pull in `depends_on` targets.
        let mut queue: VecDeque<String> = needed.iter().cloned().collect();
        while let Some(name) = queue.pop_front() {
            if let Some(svc) = self.services.get(&name) {
                for dep in svc.depends_on.keys() {
                    if needed.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Kahn's topological sort with deterministic BTreeSet ordering.
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for name in &needed {
            let deps = &self.services[name].depends_on;
            let count = deps.keys().filter(|d| needed.contains(*d)).count();
            in_degree.insert(name.clone(), count);
            for dep in deps.keys().filter(|d| needed.contains(*d)) {
                dependents.entry(dep.clone()).or_default().push(name.clone());
            }
        }

        let mut ready: BTreeSet<String> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(k, _)| k.clone())
            .collect();
        let mut ordered = Vec::with_capacity(needed.len());
        while let Some(name) = ready.pop_first() {
            ordered.push(&self.services[&name]);
            if let Some(children) = dependents.get(&name) {
                for child in children {
                    let deg = in_degree.get_mut(child).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
        }

        if ordered.len() != needed.len() {
            let stuck = in_degree
                .into_iter()
                .filter(|(_, d)| *d > 0)
                .map(|(k, _)| k)
                .min()
                .unwrap_or_else(|| "?".into());
            return Err(ComposeError::Cycle(stuck));
        }
        Ok(ordered)
    }

    /// Return services in reverse dependency order (dependents stopped first).
    pub fn shutdown_order(&self, selected: &[String], profiles: &[String]) -> Result<Vec<&ComposeService>> {
        let mut list = self.startup_order(selected, profiles)?;
        list.reverse();
        Ok(list)
    }

    /// Named volumes owned (non-external) by this project (removed on `grav compose down -v`).
    pub fn owned_volume_names(&self) -> Vec<String> {
        self.volumes
            .values()
            .filter(|v| !v.external)
            .map(|v| v.volume_name.clone())
            .collect()
    }
}

fn resolve_files(cwd: &Path, explicit: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        let mut out = Vec::with_capacity(explicit.len());
        for p in explicit {
            let full = if p.is_absolute() { p.clone() } else { cwd.join(p) };
            if !full.is_file() {
                return Err(ComposeError::Io {
                    path: full.display().to_string(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "file not found"),
                });
            }
            out.push(full);
        }
        return Ok(out);
    }
    let primary = DEFAULT_FILES
        .iter()
        .map(|f| cwd.join(f))
        .find(|p| p.is_file())
        .ok_or_else(|| ComposeError::NoComposeFile(cwd.display().to_string()))?;
    let mut out = vec![primary];
    if let Some(override_file) = OVERRIDE_FILES.iter().map(|f| cwd.join(f)).find(|p| p.is_file()) {
        out.push(override_file);
    }
    Ok(out)
}

/// Sanitize a project name per Compose rules (lowercase `[a-z0-9_-]`, starts with alphanumeric).
pub fn sanitize_project_name(raw: &str) -> String {
    let cleaned: String = raw
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let trimmed = cleaned.trim_start_matches(['-', '_']);
    if trimmed.is_empty() { "graviton".into() } else { trimmed.to_string() }
}

/// Parse a `.env` file into key-value pairs.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, raw_val)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let val = parse_env_value(raw_val.trim());
        out.push((key.to_string(), val));
    }
    out
}

fn parse_env_value(raw: &str) -> String {
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.to_string();
    }
    // Strip unquoted inline comment (` # ...`).
    if let Some(idx) = raw.find(" #") {
        return raw[..idx].trim_end().to_string();
    }
    raw.to_string()
}

/// Interpolate Docker Compose `$VAR` / `${VAR...}` / `$$` expressions.
pub fn interpolate(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if i + 1 >= bytes.len() {
            out.push('$');
            break;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'{' => {
                // Find matching `}` (handling one level of nested `${...}`).
                let start = i + 2;
                let mut depth = 1usize;
                let mut j = start;
                while j < bytes.len() && depth > 0 {
                    if bytes[j] == b'$' && j + 1 < bytes.len() && bytes[j + 1] == b'{' {
                        depth += 1;
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                if depth != 0 {
                    return Err(ComposeError::Interpolation("unclosed `${` expression".into()));
                }
                let expr = &input[start..j];
                out.push_str(&eval_braced(expr, vars)?);
                i = j + 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i + 1;
                let mut j = start + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let name = &input[start..j];
                if let Some(val) = vars.get(name) {
                    out.push_str(val);
                }
                i = j;
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn eval_braced(expr: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut name_end = 0;
    for (idx, ch) in expr.char_indices() {
        if idx == 0 && !(ch.is_ascii_alphabetic() || ch == '_') {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            name_end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    let name = &expr[..name_end];
    if name.is_empty() {
        return Err(ComposeError::Interpolation(format!("invalid variable expression `${{{expr}}}`")));
    }
    let rest = &expr[name_end..];
    let val = vars.get(name);
    if rest.is_empty() {
        return Ok(val.cloned().unwrap_or_default());
    }
    let (op, arg_raw) = if let Some(a) = rest.strip_prefix(":-") {
        (":-", a)
    } else if let Some(a) = rest.strip_prefix('-') {
        ("-", a)
    } else if let Some(a) = rest.strip_prefix(":?") {
        (":?", a)
    } else if let Some(a) = rest.strip_prefix('?') {
        ("?", a)
    } else if let Some(a) = rest.strip_prefix(":+") {
        (":+", a)
    } else if let Some(a) = rest.strip_prefix('+') {
        ("+", a)
    } else {
        return Err(ComposeError::Interpolation(format!("unsupported interpolation syntax `${{{expr}}}`")));
    };
    let arg = interpolate(arg_raw, vars)?;
    match op {
        ":-" => Ok(match val {
            Some(v) if !v.is_empty() => v.clone(),
            _ => arg,
        }),
        "-" => Ok(match val {
            Some(v) => v.clone(),
            None => arg,
        }),
        ":?" => match val {
            Some(v) if !v.is_empty() => Ok(v.clone()),
            _ => Err(ComposeError::Interpolation(if arg.is_empty() {
                format!("required variable `{name}` is not set or empty")
            } else {
                format!("variable `{name}`: {arg}")
            })),
        },
        "?" => match val {
            Some(v) => Ok(v.clone()),
            None => Err(ComposeError::Interpolation(if arg.is_empty() {
                format!("required variable `{name}` is not set")
            } else {
                format!("variable `{name}`: {arg}")
            })),
        },
        ":+" => Ok(if val.is_some_and(|v| !v.is_empty()) { arg } else { String::new() }),
        "+" => Ok(if val.is_some() { arg } else { String::new() }),
        _ => unreachable!(),
    }
}

/// Expand YAML merge keys (`<<: *anchor` or `<<: [*a, *b]`) recursively.
fn resolve_yaml_merges(node: Yaml) -> Yaml {
    match node {
        Yaml::Array(arr) => Yaml::Array(arr.into_iter().map(resolve_yaml_merges).collect()),
        Yaml::Hash(map) => {
            let mut merged = YamlHash::new();
            let mut explicit = YamlHash::new();
            for (k, v) in map {
                let v = resolve_yaml_merges(v);
                if k.as_str() == Some("<<") {
                    match v {
                        Yaml::Hash(base) => {
                            for (bk, bv) in base {
                                merged.entry(bk).or_insert(bv);
                            }
                        }
                        Yaml::Array(list) => {
                            for item in list {
                                if let Yaml::Hash(base) = item {
                                    for (bk, bv) in base {
                                        merged.entry(bk).or_insert(bv);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    explicit.insert(k, v);
                }
            }
            for (k, v) in explicit {
                merged.insert(k, v);
            }
            Yaml::Hash(merged)
        }
        other => other,
    }
}

/// Merge an override Compose root mapping into `base` (services/volumes/networks merge by key).
fn merge_root_maps(base: &mut YamlHash, overlay: YamlHash) {
    for (k, v) in overlay {
        let key_str = k.as_str().unwrap_or("");
        if matches!(key_str, "services" | "volumes" | "networks") {
            if let (Some(Yaml::Hash(b_map)), Yaml::Hash(o_map)) = (base.get_mut(&k), &v) {
                for (sk, sv) in o_map {
                    if key_str == "services" {
                        if let (Some(Yaml::Hash(b_svc)), Yaml::Hash(o_svc)) = (b_map.get_mut(sk), sv) {
                            merge_service_maps(b_svc, o_svc.clone());
                            continue;
                        }
                    }
                    b_map.insert(sk.clone(), sv.clone());
                }
                continue;
            }
        }
        base.insert(k, v);
    }
}

fn merge_service_maps(base: &mut YamlHash, overlay: YamlHash) {
    for (k, v) in overlay {
        match k.as_str() {
            Some("ports" | "volumes") => {
                if let (Some(Yaml::Array(b_arr)), Yaml::Array(o_arr)) = (base.get_mut(&k), &v) {
                    for item in o_arr {
                        if !b_arr.contains(item) {
                            b_arr.push(item.clone());
                        }
                    }
                    continue;
                }
                base.insert(k, v);
            }
            Some("environment" | "labels") => {
                if let (Some(base_val), Some(o_h)) = (base.get(&k).and_then(yaml_kv_to_hash), yaml_kv_to_hash(&v)) {
                    let mut merged = base_val;
                    for (ek, ev) in o_h {
                        merged.insert(ek, ev);
                    }
                    base.insert(k, Yaml::Hash(merged));
                    continue;
                }
                base.insert(k, v);
            }
            _ => {
                base.insert(k, v);
            }
        }
    }
}

fn yaml_kv_to_hash(v: &Yaml) -> Option<YamlHash> {
    match v {
        Yaml::Hash(h) => Some(h.clone()),
        Yaml::Array(arr) => {
            let mut h = YamlHash::new();
            for item in arr {
                let Some(s) = yaml_scalar_to_string(item) else { continue };
                if let Some((k, val)) = s.split_once('=') {
                    h.insert(Yaml::String(k.to_string()), Yaml::String(val.to_string()));
                } else {
                    h.insert(Yaml::String(s), Yaml::Null);
                }
            }
            Some(h)
        }
        Yaml::Null => Some(YamlHash::new()),
        _ => None,
    }
}

fn parse_service(
    project: &str,
    name: &str,
    h: &YamlHash,
    project_dir: &Path,
    volumes_def: &BTreeMap<String, ComposeVolumeDef>,
    vars: &BTreeMap<String, String>,
) -> Result<ComposeService> {
    let build = match yaml_get(h, "build") {
        Some(v) => Some(parse_build(name, v, project_dir, vars)?),
        None => None,
    };
    let explicit_image = yaml_get_str(h, "image");
    let image = match (&explicit_image, &build) {
        (Some(img), _) => img.clone(),
        (None, Some(_)) => format!("{project}-{name}:latest"),
        (None, None) => {
            return Err(ComposeError::Spec(format!(
                "service `{name}` must specify either `image` or `build`"
            )));
        }
    };

    let container_name = yaml_get_str(h, "container_name").unwrap_or_else(|| format!("{project}-{name}-1"));
    let hostname = yaml_get_str(h, "hostname");
    let working_dir = yaml_get_str(h, "working_dir");
    let user = yaml_get_str(h, "user");
    let platform = yaml_get_str(h, "platform").or_else(|| build.as_ref().and_then(|b| b.platform.clone()));

    let command = yaml_get(h, "command").map(|v| parse_cmd_field(name, "command", v)).transpose()?;
    let entrypoint = yaml_get(h, "entrypoint").map(|v| parse_cmd_field(name, "entrypoint", v)).transpose()?;

    // env_file first, then explicit `environment:` overrides.
    let mut environment = BTreeMap::new();
    if let Some(ef) = yaml_get(h, "env_file") {
        for (path, required) in parse_env_files(name, ef, project_dir)? {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    for (k, v) in parse_dotenv(&text) {
                        environment.insert(k, v);
                    }
                }
                Err(source) if required => return Err(ComposeError::Io { path: path.display().to_string(), source }),
                Err(_) => {}
            }
        }
    }
    if let Some(env_val) = yaml_get(h, "environment") {
        for (k, v) in parse_key_value_field(name, "environment", env_val, Some(vars))? {
            environment.insert(k, v);
        }
    }

    let mut ports = Vec::new();
    if let Some(p_val) = yaml_get(h, "ports") {
        let Yaml::Array(arr) = p_val else {
            return Err(ComposeError::Spec(format!("service `{name}`: `ports` must be a list")));
        };
        for item in arr {
            ports.push(parse_port_entry(name, item)?);
        }
    }

    let mut volumes = Vec::new();
    if let Some(v_val) = yaml_get(h, "volumes") {
        let Yaml::Array(arr) = v_val else {
            return Err(ComposeError::Spec(format!("service `{name}`: `volumes` must be a list")));
        };
        for item in arr {
            volumes.push(parse_volume_entry(name, item, project_dir, volumes_def)?);
        }
    }

    let depends_on = match yaml_get(h, "depends_on") {
        Some(v) => parse_depends_on(name, v)?,
        None => BTreeMap::new(),
    };

    let restart = yaml_get_str(h, "restart").map(|r| parse_restart(name, &r)).transpose()?;

    let mut labels = BTreeMap::new();
    labels.insert("com.docker.compose.project".into(), project.into());
    labels.insert("com.docker.compose.service".into(), name.into());
    if let Some(l_val) = yaml_get(h, "labels") {
        for (k, v) in parse_key_value_field(name, "labels", l_val, None)? {
            labels.insert(k, v);
        }
    }

    let (cpus, memory_mib) = parse_resources(name, h)?;
    let dns_aliases = parse_network_aliases(h);
    let profiles = parse_string_list(yaml_get(h, "profiles"));
    let healthcheck = match yaml_get(h, "healthcheck").filter(|y| !matches!(y, Yaml::Null)) {
        Some(v) => Some(parse_healthcheck(name, v)?),
        None => None,
    };

    Ok(ComposeService {
        name: name.to_string(),
        container_name,
        image,
        build,
        command,
        entrypoint,
        environment,
        ports,
        volumes,
        depends_on,
        restart,
        working_dir,
        user,
        hostname,
        platform,
        cpus,
        memory_mib,
        labels,
        dns_aliases,
        profiles,
        healthcheck,
    })
}

fn parse_healthcheck(service: &str, v: &Yaml) -> Result<Healthcheck> {
    let Some(h) = v.as_hash() else {
        return Err(ComposeError::Spec(format!("service `{service}`: `healthcheck` must be a mapping")));
    };
    let disabled = yaml_get(h, "disable")
        .and_then(yaml_scalar_to_string)
        .is_some_and(|s| s.eq_ignore_ascii_case("true"));
    if disabled {
        return Ok(Healthcheck::disabled());
    }

    let test = match yaml_get(h, "test") {
        None | Some(Yaml::Null) => Vec::new(),
        Some(Yaml::String(s)) => {
            if s.trim().eq_ignore_ascii_case("NONE") {
                return Ok(Healthcheck::disabled());
            }
            vec!["CMD-SHELL".into(), s.clone()]
        }
        Some(Yaml::Array(arr)) => {
            let mut items = Vec::with_capacity(arr.len());
            for item in arr {
                let s = yaml_scalar_to_string(item).ok_or_else(|| {
                    ComposeError::Spec(format!("service `{service}`: `healthcheck.test` items must be strings"))
                })?;
                items.push(s);
            }
            if items.is_empty() {
                return Err(ComposeError::Spec(format!("service `{service}`: `healthcheck.test` cannot be empty")));
            }
            if items[0].eq_ignore_ascii_case("NONE") {
                return Ok(Healthcheck::disabled());
            }
            if items[0] != "CMD" && items[0] != "CMD-SHELL" {
                items.insert(0, "CMD".into());
            }
            items
        }
        _ => {
            return Err(ComposeError::Spec(format!(
                "service `{service}`: `healthcheck.test` must be a string or list"
            )));
        }
    };

    let mut hc = Healthcheck {
        test,
        interval_secs: 0,
        timeout_secs: 0,
        start_period_secs: 0,
        retries: 0,
    };

    let parse_dur = |field: &str, allow_zero: bool| -> Result<Option<u64>> {
        let Some(val) = yaml_get(h, field).filter(|y| !matches!(y, Yaml::Null)) else {
            return Ok(None);
        };
        let raw = yaml_scalar_to_string(val).ok_or_else(|| {
            ComposeError::Spec(format!("service `{service}`: invalid `healthcheck.{field}`"))
        })?;
        let secs = parse_duration_secs(&raw).map_err(|e| {
            ComposeError::Spec(format!("service `{service}`: invalid `healthcheck.{field}`: {e}"))
        })?;
        if !allow_zero && secs == 0 {
            return Err(ComposeError::Spec(format!(
                "service `{service}`: `healthcheck.{field}` must be greater than 0s"
            )));
        }
        Ok(Some(secs))
    };

    if let Some(v) = parse_dur("interval", false)? {
        hc.interval_secs = v;
    }
    if let Some(v) = parse_dur("timeout", false)? {
        hc.timeout_secs = v;
    }
    if let Some(v) = parse_dur("start_period", true)? {
        hc.start_period_secs = v;
    }
    let _ = parse_dur("start_interval", false)?;
    if let Some(val) = yaml_get(h, "retries").filter(|y| !matches!(y, Yaml::Null)) {
        let raw = yaml_scalar_to_string(val).ok_or_else(|| {
            ComposeError::Spec(format!("service `{service}`: invalid `healthcheck.retries`"))
        })?;
        let n: u32 = raw.trim().parse().map_err(|_| {
            ComposeError::Spec(format!("service `{service}`: invalid `healthcheck.retries` value `{raw}`"))
        })?;
        if n == 0 {
            return Err(ComposeError::Spec(format!(
                "service `{service}`: `healthcheck.retries` must be at least 1"
            )));
        }
        hc.retries = n;
    }

    if !hc.test.is_empty() {
        Ok(hc.normalized())
    } else {
        Ok(hc)
    }
}

fn parse_build(service: &str, v: &Yaml, project_dir: &Path, vars: &BTreeMap<String, String>) -> Result<ComposeBuild> {
    match v {
        Yaml::String(ctx) => Ok(ComposeBuild {
            context: resolve_path(project_dir, ctx),
            dockerfile: None,
            args: BTreeMap::new(),
            target: None,
            platform: None,
        }),
        Yaml::Hash(h) => {
            let ctx = yaml_get_str(h, "context").unwrap_or_else(|| ".".into());
            let dockerfile = yaml_get_str(h, "dockerfile");
            let target = yaml_get_str(h, "target");
            let platform = yaml_get_str(h, "platform");
            let args = match yaml_get(h, "args") {
                Some(a) => parse_key_value_field(service, "build.args", a, Some(vars))?,
                None => BTreeMap::new(),
            };
            Ok(ComposeBuild {
                context: resolve_path(project_dir, &ctx),
                dockerfile,
                args,
                target,
                platform,
            })
        }
        _ => Err(ComposeError::Spec(format!("service `{service}`: `build` must be a path or mapping"))),
    }
}

fn parse_cmd_field(service: &str, field: &str, v: &Yaml) -> Result<Vec<String>> {
    match v {
        Yaml::String(s) => Ok(split_shell_words(s)),
        Yaml::Array(arr) => arr
            .iter()
            .map(|item| {
                yaml_scalar_to_string(item).ok_or_else(|| {
                    ComposeError::Spec(format!("service `{service}`: `{field}` elements must be strings"))
                })
            })
            .collect(),
        Yaml::Null => Ok(Vec::new()),
        _ => Err(ComposeError::Spec(format!("service `{service}`: `{field}` must be a string or list"))),
    }
}

fn parse_env_files(service: &str, v: &Yaml, project_dir: &Path) -> Result<Vec<(PathBuf, bool)>> {
    match v {
        Yaml::String(p) => Ok(vec![(resolve_path(project_dir, p), true)]),
        Yaml::Array(arr) => {
            let mut out = Vec::new();
            for item in arr {
                match item {
                    Yaml::String(p) => out.push((resolve_path(project_dir, p), true)),
                    Yaml::Hash(h) => {
                        let p = yaml_get_str(h, "path")
                            .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: env_file entry missing `path`")))?;
                        let required = match yaml_get(h, "required") {
                            Some(Yaml::Boolean(b)) => *b,
                            _ => true,
                        };
                        out.push((resolve_path(project_dir, &p), required));
                    }
                    _ => return Err(ComposeError::Spec(format!("service `{service}`: invalid `env_file` item"))),
                }
            }
            Ok(out)
        }
        _ => Err(ComposeError::Spec(format!("service `{service}`: `env_file` must be a path or list"))),
    }
}

fn parse_key_value_field(
    service: &str,
    field: &str,
    v: &Yaml,
    host_lookup: Option<&BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    match v {
        Yaml::Hash(h) => {
            for (k, val) in h {
                let Some(key) = yaml_scalar_to_string(k) else { continue };
                match val {
                    Yaml::Null => {
                        if let Some(v) = host_lookup.and_then(|m| m.get(&key)) {
                            out.insert(key, v.clone());
                        }
                    }
                    other => {
                        if let Some(s) = yaml_scalar_to_string(other) {
                            out.insert(key, s);
                        }
                    }
                }
            }
        }
        Yaml::Array(arr) => {
            for item in arr {
                let Some(s) = yaml_scalar_to_string(item) else { continue };
                if let Some((k, val)) = s.split_once('=') {
                    out.insert(k.to_string(), val.to_string());
                } else if let Some(val) = host_lookup.and_then(|m| m.get(&s)) {
                    out.insert(s, val.clone());
                }
            }
        }
        Yaml::Null => {}
        _ => return Err(ComposeError::Spec(format!("service `{service}`: `{field}` must be a list or mapping"))),
    }
    Ok(out)
}

fn parse_port_entry(service: &str, v: &Yaml) -> Result<String> {
    match v {
        Yaml::String(s) => Ok(s.trim().to_string()),
        Yaml::Integer(n) => Ok(n.to_string()),
        Yaml::Hash(h) => {
            let target = yaml_get(h, "target")
                .and_then(yaml_scalar_to_string)
                .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: port object missing `target`")))?;
            let published = yaml_get(h, "published").and_then(yaml_scalar_to_string).unwrap_or_else(|| target.clone());
            let host_ip = yaml_get_str(h, "host_ip");
            let protocol = yaml_get_str(h, "protocol");
            let mut spec = match host_ip {
                Some(ip) if ip.contains(':') && !ip.starts_with('[') => format!("[{ip}]:{published}:{target}"),
                Some(ip) => format!("{ip}:{published}:{target}"),
                None => format!("{published}:{target}"),
            };
            if let Some(proto) = protocol {
                if !proto.eq_ignore_ascii_case("tcp") {
                    spec.push('/');
                    spec.push_str(&proto.to_ascii_lowercase());
                }
            }
            Ok(spec)
        }
        _ => Err(ComposeError::Spec(format!("service `{service}`: invalid port entry"))),
    }
}

fn parse_volume_entry(
    service: &str,
    v: &Yaml,
    project_dir: &Path,
    volumes_def: &BTreeMap<String, ComposeVolumeDef>,
) -> Result<String> {
    let (kind, source, target, read_only) = match v {
        Yaml::String(s) => {
            let parts: Vec<&str> = s.split(':').collect();
            if parts.len() < 2 || parts.len() > 3 {
                return Err(ComposeError::Spec(format!(
                    "service `{service}`: volume `{s}` must be SOURCE:TARGET[:ro]"
                )));
            }
            let ro = parts.get(2).is_some_and(|opts| opts.split(',').any(|o| o.trim() == "ro"));
            let src = parts[0];
            let kind = if src.starts_with('.') || src.starts_with('/') || src.starts_with('~') {
                "bind"
            } else {
                "volume"
            };
            (kind.to_string(), src.to_string(), parts[1].to_string(), ro)
        }
        Yaml::Hash(h) => {
            let kind = yaml_get_str(h, "type").unwrap_or_else(|| "volume".into());
            let source = yaml_get_str(h, "source")
                .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: volume object missing `source`")))?;
            let target = yaml_get_str(h, "target")
                .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: volume object missing `target`")))?;
            let ro = matches!(yaml_get(h, "read_only"), Some(Yaml::Boolean(true)));
            (kind, source, target, ro)
        }
        _ => return Err(ComposeError::Spec(format!("service `{service}`: invalid volume entry"))),
    };

    let resolved_src = match kind.as_str() {
        "bind" => {
            if source.starts_with('~') {
                source
            } else {
                resolve_path(project_dir, &source).display().to_string()
            }
        }
        "volume" => {
            if let Some(def) = volumes_def.get(&source) {
                def.volume_name.clone()
            } else {
                source
            }
        }
        other => {
            return Err(ComposeError::Spec(format!(
                "service `{service}`: unsupported volume type `{other}` (use `bind` or `volume`)"
            )));
        }
    };

    Ok(if read_only {
        format!("{resolved_src}:{target}:ro")
    } else {
        format!("{resolved_src}:{target}")
    })
}

fn parse_depends_on(service: &str, v: &Yaml) -> Result<BTreeMap<String, DependsCondition>> {
    let mut out = BTreeMap::new();
    match v {
        Yaml::Array(arr) => {
            for item in arr {
                if let Some(dep) = item.as_str() {
                    out.insert(dep.to_string(), DependsCondition::ServiceStarted);
                }
            }
        }
        Yaml::Hash(h) => {
            for (k, val) in h {
                let Some(dep) = k.as_str() else { continue };
                let cond = match val.as_hash().and_then(|dh| yaml_get_str(dh, "condition")).as_deref() {
                    None | Some("service_started") => DependsCondition::ServiceStarted,
                    Some("service_completed_successfully") => DependsCondition::ServiceCompletedSuccessfully,
                    Some("service_healthy") => DependsCondition::ServiceHealthy,
                    Some(other) => {
                        return Err(ComposeError::Spec(format!(
                            "service `{service}`: unsupported depends_on condition `{other}`"
                        )));
                    }
                };
                out.insert(dep.to_string(), cond);
            }
        }
        Yaml::Null => {}
        _ => return Err(ComposeError::Spec(format!("service `{service}`: `depends_on` must be a list or mapping"))),
    }
    Ok(out)
}

fn parse_restart(service: &str, raw: &str) -> Result<RestartPolicy> {
    let s = raw.trim().to_ascii_lowercase();
    if s == "no" || s == "never" {
        Ok(RestartPolicy::No)
    } else if s == "always" || s == "unless-stopped" {
        Ok(RestartPolicy::Always)
    } else if s == "on-failure" || s.starts_with("on-failure:") {
        Ok(RestartPolicy::OnFailure)
    } else {
        Err(ComposeError::Spec(format!(
            "service `{service}`: unknown restart policy `{raw}`"
        )))
    }
}

fn parse_resources(service: &str, h: &YamlHash) -> Result<(Option<u32>, Option<u64>)> {
    let limits = yaml_get(h, "deploy")
        .and_then(Yaml::as_hash)
        .and_then(|d| yaml_get(d, "resources"))
        .and_then(Yaml::as_hash)
        .and_then(|r| yaml_get(r, "limits"))
        .and_then(Yaml::as_hash);

    let cpus_raw = yaml_get(h, "cpus").or_else(|| limits.and_then(|l| yaml_get(l, "cpus")));
    let cpus = match cpus_raw {
        Some(v) => {
            let s = yaml_scalar_to_string(v)
                .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: invalid `cpus`")))?;
            let f: f64 = s
                .parse()
                .map_err(|_| ComposeError::Spec(format!("service `{service}`: invalid `cpus` value `{s}`")))?;
            if f <= 0.0 {
                return Err(ComposeError::Spec(format!("service `{service}`: `cpus` must be positive")));
            }
            Some(f.ceil() as u32)
        }
        None => None,
    };

    let mem_raw = yaml_get(h, "mem_limit").or_else(|| limits.and_then(|l| yaml_get(l, "memory")));
    let memory_mib = match mem_raw {
        Some(v) => {
            let s = yaml_scalar_to_string(v)
                .ok_or_else(|| ComposeError::Spec(format!("service `{service}`: invalid memory limit")))?;
            Some(parse_memory_mib(&s).ok_or_else(|| {
                ComposeError::Spec(format!("service `{service}`: invalid memory limit `{s}` (e.g. 512m, 2g)"))
            })?)
        }
        None => None,
    };

    Ok((cpus, memory_mib))
}

/// Parse Docker memory strings (`512m`, `2g`, `1024mb`, `1073741824`) into MiB (min 128 MiB).
pub fn parse_memory_mib(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let num: f64 = s[..split].parse().ok()?;
    if num <= 0.0 || !num.is_finite() {
        return None;
    }
    let unit = s[split..].trim();
    let bytes = match unit {
        "" | "b" => num,
        "k" | "kb" | "kib" => num * 1024.0,
        "m" | "mb" | "mib" => num * 1024.0 * 1024.0,
        "g" | "gb" | "gib" => num * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(((bytes / (1024.0 * 1024.0)).ceil() as u64).max(128))
}

fn parse_network_aliases(h: &YamlHash) -> Vec<String> {
    let mut aliases = Vec::new();
    if let Some(Yaml::Hash(nets)) = yaml_get(h, "networks") {
        for (_, net_cfg) in nets {
            if let Some(nh) = net_cfg.as_hash() {
                for alias in parse_string_list(yaml_get(nh, "aliases")) {
                    if !aliases.contains(&alias) {
                        aliases.push(alias);
                    }
                }
            }
        }
    }
    aliases
}

fn parse_string_list(v: Option<&Yaml>) -> Vec<String> {
    let Some(Yaml::Array(arr)) = v else { return Vec::new() };
    arr.iter().filter_map(yaml_scalar_to_string).collect()
}

fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        clean_join(base, p)
    }
}

fn clean_join(base: &Path, rel: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for c in rel.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Split a shell command string into words, honouring single/double quotes and backslashes.
pub fn split_shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut in_word = false;

    for ch in line.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            in_word = true;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            in_word = true;
        } else if ch.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(ch);
            in_word = true;
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn yaml_get<'a>(h: &'a YamlHash, key: &str) -> Option<&'a Yaml> {
    h.get(&Yaml::String(key.to_string()))
}

fn yaml_get_str(h: &YamlHash, key: &str) -> Option<String> {
    yaml_get(h, key).and_then(yaml_scalar_to_string)
}

fn yaml_scalar_to_string(v: &Yaml) -> Option<String> {
    match v {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(n) => Some(n.to_string()),
        Yaml::Real(s) => Some(s.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_compose_expressions_and_escapes() {
        let vars = BTreeMap::from([
            ("TAG".into(), "17-alpine".into()),
            ("EMPTY".into(), "".into()),
        ]);
        assert_eq!(interpolate("postgres:${TAG}", &vars).unwrap(), "postgres:17-alpine");
        assert_eq!(interpolate("$TAG/$$/literal", &vars).unwrap(), "17-alpine/$/literal");
        assert_eq!(interpolate("${MISSING:-fallback}", &vars).unwrap(), "fallback");
        assert_eq!(interpolate("${EMPTY:-fallback}", &vars).unwrap(), "fallback");
        assert_eq!(interpolate("${EMPTY-fallback}", &vars).unwrap(), "");
        assert_eq!(interpolate("${TAG:+set}", &vars).unwrap(), "set");
        assert_eq!(interpolate("${MISSING:+set}", &vars).unwrap(), "");
        assert!(interpolate("${MISSING:?must provide}", &vars).is_err());
        assert!(interpolate("${EMPTY:?must be non-empty}", &vars).is_err());
        assert_eq!(interpolate("${EMPTY?unset only}", &vars).unwrap(), "");
    }

    #[test]
    fn parses_dotenv_and_memory_and_shell_words() {
        let env = parse_dotenv(
            r#"
            # comment
            FOO=bar
            export QUOTED="hello\nworld"
            SINGLE='a # b'
            INLINE=val # trailing comment
            "#,
        );
        assert_eq!(
            env,
            vec![
                ("FOO".into(), "bar".into()),
                ("QUOTED".into(), "hello\nworld".into()),
                ("SINGLE".into(), "a # b".into()),
                ("INLINE".into(), "val".into()),
            ]
        );
        assert_eq!(parse_memory_mib("512m"), Some(512));
        assert_eq!(parse_memory_mib("2g"), Some(2048));
        assert_eq!(parse_memory_mib("64m"), Some(128)); // clamped to MIN_MEMORY_MIB
        assert_eq!(split_shell_words("sh -c 'echo \"hi there\"'"), vec!["sh", "-c", "echo \"hi there\""]);
        assert_eq!(sanitize_project_name("My App_2!"), "myapp_2");
    }

    #[test]
    fn parses_compose_healthcheck_variants() {
        let dir = std::env::temp_dir().join(format!("graviton-compose-hc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("compose.yaml"),
            r#"
services:
  db:
    image: postgres:17-alpine
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 5s
      timeout: 3s
      start_period: 10s
      retries: 4
  api:
    image: alpine
    depends_on:
      db:
        condition: service_healthy
    healthcheck:
      test: "wget -qO- http://localhost:8080/health || exit 1"
  worker:
    image: alpine
    healthcheck:
      disable: true
  tuner:
    image: alpine
    healthcheck:
      interval: 2s
"#,
        )
        .unwrap();

        let proj = ComposeProject::load_with_env(&dir, &LoadOptions::default(), &BTreeMap::new()).unwrap();
        let db = &proj.services["db"];
        assert_eq!(
            db.healthcheck,
            Some(Healthcheck {
                test: vec!["CMD-SHELL".into(), "pg_isready -U postgres".into()],
                interval_secs: 5,
                timeout_secs: 3,
                start_period_secs: 10,
                retries: 4,
            })
        );
        let api = &proj.services["api"];
        assert_eq!(
            api.healthcheck,
            Some(Healthcheck {
                test: vec!["CMD-SHELL".into(), "wget -qO- http://localhost:8080/health || exit 1".into()],
                interval_secs: 30,
                timeout_secs: 30,
                start_period_secs: 0,
                retries: 3,
            })
        );
        assert_eq!(api.depends_on["db"], DependsCondition::ServiceHealthy);
        assert_eq!(proj.services["worker"].healthcheck, Some(Healthcheck::disabled()));
        // Timing-only override keeps unset fields as 0 so daemon merges image test cleanly.
        assert_eq!(
            proj.services["tuner"].healthcheck,
            Some(Healthcheck { test: vec![], interval_secs: 2, timeout_secs: 0, start_period_secs: 0, retries: 0 })
        );

        for bad in [
            "services:\n  a:\n    image: alpine\n    healthcheck:\n      interval: 0s\n",
            "services:\n  a:\n    image: alpine\n    healthcheck:\n      retries: 0\n",
        ] {
            fs::write(dir.join("compose.yaml"), bad).unwrap();
            assert!(ComposeProject::load_with_env(&dir, &LoadOptions::default(), &BTreeMap::new()).is_err());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_merges_and_toposorts_full_compose_project() {
        let dir = std::env::temp_dir().join(format!("graviton-compose-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".env"), "PG_VER=17-alpine\nHOST_PASS=from-dotenv\n").unwrap();
        fs::write(dir.join("app.env"), "FROM_FILE=1\nOVERRIDE_ME=old\n").unwrap();
        fs::write(
            dir.join("compose.yaml"),
            r#"
name: Storefront

x-common: &common
  restart: unless-stopped
  labels:
    team: platform

services:
  db:
    <<: *common
    image: postgres:${PG_VER}
    environment:
      POSTGRES_PASSWORD: ${HOST_PASS}
    volumes:
      - pgdata:/var/lib/postgresql/data
      - type: bind
        source: ./init
        target: /docker-entrypoint-initdb.d
        read_only: true
    ports:
      - "5432:5432"
    deploy:
      resources:
        limits:
          cpus: "1.5"
          memory: 1536m
    networks:
      backend:
        aliases:
          - primary-db

  migrate:
    image: alpine:3.22
    command: "sh -c 'echo migrated'"
    depends_on:
      - db

  api:
    <<: *common
    build:
      context: ./api
      args:
        RUST_ENV: prod
    env_file: app.env
    environment:
      - OVERRIDE_ME=new
      - HOST_PASS
    ports:
      - target: 8000
        published: 8080
        host_ip: 0.0.0.0
      - "5353:53/udp"
    depends_on:
      db:
        condition: service_started
      migrate:
        condition: service_completed_successfully
    platform: linux/amd64

  debug:
    image: alpine:3.22
    profiles: [dev]

volumes:
  pgdata:
  shared_ext:
    external: true
"#,
        )
        .unwrap();
        fs::write(
            dir.join("compose.override.yaml"),
            r#"
services:
  api:
    environment:
      EXTRA_OVERRIDE: "yes"
"#,
        )
        .unwrap();

        let host_env = BTreeMap::from([("HOST_PASS".into(), "from-host".into())]);
        let proj = ComposeProject::load_with_env(&dir, &LoadOptions::default(), &host_env).unwrap();
        assert_eq!(proj.name, "storefront");
        assert_eq!(proj.files.len(), 2);

        // Default startup order excludes profiled `debug` service and puts db -> migrate -> api.
        let order: Vec<&str> = proj.startup_order(&[], &[]).unwrap().into_iter().map(|s| s.name.as_str()).collect();
        assert_eq!(order, vec!["db", "migrate", "api"]);

        // Profile `dev` includes `debug`.
        let with_dev: Vec<&str> = proj
            .startup_order(&[], &["dev".into()])
            .unwrap()
            .into_iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(with_dev.contains(&"debug"));

        let db = &proj.services["db"];
        assert_eq!(db.image, "postgres:17-alpine");
        assert_eq!(db.container_name, "storefront-db-1");
        assert_eq!(db.environment["POSTGRES_PASSWORD"], "from-host"); // host overrides .env
        assert_eq!(db.restart, Some(RestartPolicy::Always));
        assert_eq!(db.cpus, Some(2));
        assert_eq!(db.memory_mib, Some(1536));
        assert_eq!(db.dns_aliases, vec!["primary-db"]);
        assert_eq!(db.volumes[0], "storefront_pgdata:/var/lib/postgresql/data");
        assert!(db.volumes[1].ends_with("/init:/docker-entrypoint-initdb.d:ro"));
        assert_eq!(db.labels["team"], "platform");
        assert_eq!(db.labels["com.docker.compose.project"], "storefront");

        let api = &proj.services["api"];
        assert_eq!(api.image, "storefront-api:latest");
        assert!(api.build.is_some());
        assert_eq!(api.platform.as_deref(), Some("linux/amd64"));
        assert_eq!(api.environment["FROM_FILE"], "1");
        assert_eq!(api.environment["OVERRIDE_ME"], "new");
        assert_eq!(api.environment["HOST_PASS"], "from-host");
        assert_eq!(api.ports, vec!["0.0.0.0:8080:8000", "5353:53/udp"]);
        assert_eq!(api.depends_on["migrate"], DependsCondition::ServiceCompletedSuccessfully);

        assert_eq!(proj.owned_volume_names(), vec!["storefront_pgdata"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_dependency_cycles() {
        let dir = std::env::temp_dir().join(format!("graviton-compose-cycle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("compose.yaml"),
            r#"
services:
  a:
    image: alpine
    depends_on: [b]
  b:
    image: alpine
    depends_on: [a]
"#,
        )
        .unwrap();
        let proj = ComposeProject::load_with_env(&dir, &LoadOptions::default(), &BTreeMap::new()).unwrap();
        assert!(matches!(proj.startup_order(&[], &[]), Err(ComposeError::Cycle(_))));
        let _ = fs::remove_dir_all(&dir);
    }
}
