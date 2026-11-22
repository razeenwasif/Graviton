//! `grav build`: images from Dockerfiles, without Docker.
//!
//! gravitond parses the Dockerfile and walks the stages the target needs,
//! giving every step a cache key (the previous key, the step as written, and
//! for COPY/ADD a hash of the files, heredocs or downloaded URLs it takes).
//! Steps with a cached layer reuse it without running anything. At the first
//! miss in a stage the cache is off for the rest of that stage (as in Docker),
//! and a builder VM boots: one VM for the whole build, with the build context
//! shared read-only, persistent `RUN --mount=type=cache` directories in the
//! `cache` share, per-build `RUN --mount=type=secret` files in a `0700`
//! read-only `secrets` share, downloaded `ADD <url>` files in `staged`, and an
//! `out` folder for committed layers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use graviton_core::{ImageReference, paths};
use graviton_image::dockerfile::{Command, Dockerfile, Heredoc, Kind, RunMount, expand, parse_octal_mode};
use graviton_image::{Digest, Layer, LocalImage};
use graviton_proto::build::{Ignore, expand_source, walk};
use graviton_proto::{
    AGENT_VSOCK_PORT, AgentEvent, AgentRequest, BuildSpec, ExecMount, ExecMountKind, ImageSummary, InlineFile, LayerInfo, StagedFile, encode_line,
};
use graviton_vmm::apple::{AppleHypervisor, AppleVm};
use graviton_vmm::{NetworkMode, SharedDirectory, VmConfig, VmHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::runtime::{Engine, Events, LineBuffer, new_id, now_secs, read_event_unbuffered};

const GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuiltLayer {
    digest: String,
    diff_id: String,
    size: u64,
    media_type: String,
}

/// One cached step: the layer it made (`None`: it changed no files) and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedStep {
    layer: Option<BuiltLayer>,
    /// Unix seconds; the image's `created` is its newest step's, so an
    /// unchanged rebuild gives the same image.
    created: u64,
}

/// `build-cache.json`: step key → what the step made.
#[derive(Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    version: u32,
    steps: HashMap<String, CachedStep>,
}

impl Cache {
    fn path() -> PathBuf {
        paths::support_dir().join("build-cache.json")
    }
    fn load() -> Self {
        fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice::<Self>(&b).ok()).filter(|c| c.version == 1).unwrap_or(Self { version: 1, ..Self::default() })
    }
    fn save(&self) {
        let tmp = Self::path().with_extension("json.tmp");
        if fs::write(&tmp, serde_json::to_vec(self).expect("cache serialises")).is_ok() {
            let _ = fs::rename(tmp, Self::path());
        }
    }
}

/// Layer digests the build cache keeps, and how many steps it remembers.
pub(crate) fn cache_contents() -> (std::collections::HashSet<String>, u32) {
    let cache = Cache::load();
    let layers = cache.steps.values().filter_map(|s| s.layer.as_ref().map(|l| l.digest.clone())).collect();
    (layers, cache.steps.len() as u32)
}

/// Forget every cached step (`grav prune`); their layers are then unreferenced.
pub(crate) fn clear_cache() {
    Cache { version: 1, ..Cache::default() }.save();
}

/// Persistent `RUN --mount=type=cache` directories live here across builds.
fn cache_mounts_dir() -> PathBuf {
    paths::support_dir().join("build-cache-mounts")
}

/// Total disk bytes used by persistent `RUN --mount=type=cache` directories.
pub(crate) fn cache_mounts_bytes() -> u64 {
    dir_size(&cache_mounts_dir())
}

/// Remove all persistent `RUN --mount=type=cache` directories (`grav prune`).
pub(crate) fn clear_cache_mounts() {
    let _ = fs::remove_dir_all(cache_mounts_dir());
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = fs::read_dir(&p) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if let Ok(m) = fs::symlink_metadata(&path) {
                if m.is_dir() {
                    stack.push(path);
                } else if m.is_file() {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// Stable, filesystem-safe directory name for a `RUN --mount=type=cache` key.
fn cache_mount_key(id_or_target: &str) -> String {
    let slug: String = id_or_target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    let slug = slug.trim_matches('_');
    let digest = hash(&["cache-mount", id_or_target]);
    if slug.is_empty() {
        digest[..16].to_string()
    } else {
        format!("{slug}-{}", &digest[..12])
    }
}

/// Ensure a persistent cache mount folder exists on the host with permissions
/// usable by both root and non-root container users over virtiofs, inside a
/// `0700` parent owned by this Mac user.
fn ensure_cache_mount_dir(key: &str) -> Result<PathBuf> {
    let root = cache_mounts_dir();
    fs::create_dir_all(&root)?;
    let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
    let dir = root.join(key);
    if !dir.is_dir() {
        fs::create_dir_all(&dir)?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o777));
    }
    Ok(dir)
}

/// Where a stage has got to.
#[derive(Clone)]
struct State {
    architecture: String,
    layers: Vec<BuiltLayer>,
    /// The base image's whole OCI config; `config` is rewritten at the end.
    base_config: Value,
    env: Vec<String>,
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    workdir: String,
    user: Option<String>,
    shell: Vec<String>,
    exposed: BTreeSet<String>,
    labels: BTreeMap<String, String>,
    volumes: BTreeSet<String>,
    stop_signal: Option<String>,
    history: Vec<Value>,
    key: String,
    /// The stage's ARGs in scope, with their values.
    args: BTreeMap<String, String>,
    /// When its newest file-changing step was made (Unix seconds).
    created: Option<u64>,
}

impl State {
    fn vars(&self) -> BTreeMap<String, String> {
        let mut v = self.args.clone();
        for e in &self.env {
            if let Some((k, val)) = e.split_once('=') {
                v.insert(k.into(), val.into());
            }
        }
        v
    }

    fn set_env(&mut self, key: &str, value: &str) {
        self.env.retain(|e| e.split_once('=').map_or(e.as_str(), |(k, _)| k) != key);
        self.env.push(format!("{key}={value}"));
    }
}

fn hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The builder VM, booted on the first cache miss.
struct Builder {
    vm: Arc<AppleVm>,
    writer: File,
    events: Events,
    out_dir: PathBuf,
    /// Containers prepared in the VM.
    prepared: BTreeSet<String>,
    seq: u32,
}

impl Builder {
    fn request(&mut self, req: &AgentRequest) -> Result<AgentEvent> {
        self.writer.write_all(&encode_line(req)).context("talking to the builder VM")?;
        match self.events.next()? {
            AgentEvent::Error { message } => bail!("{message}"),
            event => Ok(event),
        }
    }

    fn shutdown(self) {
        let mut w = self.writer;
        let _ = w.write_all(&encode_line(&AgentRequest::Shutdown));
        if self.vm.wait_stopped(Duration::from_secs(5)).is_err() {
            let _ = self.vm.kill();
        }
    }
}

/// Pre-prepared copy inputs (local paths, expanded inline heredocs, staged URL files).
struct PreparedCopy {
    local_sources: Vec<String>,
    inline: Vec<InlineFile>,
    staged: Vec<StagedFile>,
    chmod: Option<u32>,
    chown: Option<String>,
    dest: String,
    source_key: String,
}

pub fn build(engine: &Engine, spec: &BuildSpec, log: &mut dyn FnMut(String)) -> Result<ImageSummary> {
    let t0 = Instant::now();
    let context = PathBuf::from(&spec.context);
    if !context.is_dir() {
        bail!("{} isn't a folder", context.display());
    }
    let dockerfile_path = match &spec.dockerfile {
        Some(f) if Path::new(f).is_absolute() => PathBuf::from(f),
        Some(f) => context.join(f),
        None => context.join("Dockerfile"),
    };
    let text = fs::read_to_string(&dockerfile_path).with_context(|| format!("reading {}", dockerfile_path.display()))?;
    let df = Dockerfile::parse(&text)?;
    let ignore_lines = fs::read_to_string(context.join(".dockerignore")).map(|t| Ignore::lines(&t)).unwrap_or_default();
    let ignore = Ignore::new(&ignore_lines);

    let mut build_args: BTreeMap<String, String> = BTreeMap::new();
    for pair in &spec.build_args {
        let (k, v) = pair.split_once('=').ok_or_else(|| anyhow!("--build-arg {pair} should be KEY=value"))?;
        build_args.insert(k.into(), v.into());
    }
    let global: BTreeMap<String, String> = df
        .global_args
        .iter()
        .filter_map(|(k, d)| build_args.get(k).cloned().or_else(|| d.clone()).map(|v| (k.clone(), v)))
        .collect();

    let target = match &spec.target {
        Some(t) => df.stage_index(t).ok_or_else(|| anyhow!("no stage named {t} in the Dockerfile"))?,
        None => df.stages.len() - 1,
    };
    // Stages the target needs, through FROM <stage>, COPY --from=<stage>, and RUN --mount=type=bind,from=<stage>.
    let mut needed = BTreeSet::from([target]);
    let mut queue = vec![target];
    while let Some(i) = queue.pop() {
        let stage = &df.stages[i];
        let mut deps: Vec<usize> = df.stage_index(&expand(&stage.base, &global)).filter(|d| *d < i).into_iter().collect();
        for ins in &stage.instructions {
            match &ins.kind {
                Kind::Copy { from: Some(f), .. } => {
                    deps.extend(df.stage_index(f).filter(|d| *d < i));
                }
                Kind::Run { mounts, .. } => {
                    for m in mounts {
                        if let RunMount::Bind { from: Some(f), .. } = m {
                            deps.extend(df.stage_index(f).filter(|d| *d < i));
                        }
                    }
                }
                _ => {}
            }
        }
        for d in deps {
            if needed.insert(d) {
                queue.push(d);
            }
        }
    }

    let reference = image_reference(spec, &context)?;
    let steps: usize = needed.iter().map(|&i| df.stages[i].instructions.len() + 1).sum();
    log(format!("Building {} from {} ({} stage{}, {steps} steps)", display(&reference), dockerfile_path.display(), needed.len(), if needed.len() == 1 { "" } else { "s" }));

    // Per-build working folder (always deleted on exit so secrets & downloads never linger).
    let build_id = new_id();
    let build_dir = paths::support_dir().join("builds").join(&build_id);
    let out_dir = build_dir.join("out");
    let staged_dir = build_dir.join("staged");
    let secrets_dir = build_dir.join("secrets");
    fs::create_dir_all(&out_dir)?;
    fs::create_dir_all(&staged_dir)?;
    fs::create_dir_all(&secrets_dir)?;
    let _ = fs::set_permissions(&secrets_dir, fs::Permissions::from_mode(0o700));

    // Write build secrets into the 0700 per-build secrets share; file mode 0644
    // inside that private directory lets non-root container USERs read them over virtiofs.
    let mut secrets_map: HashMap<String, (String, Vec<u8>)> = HashMap::new();
    for s in &spec.secrets {
        let file_name = hash(&["secret", &s.id]);
        let path = secrets_dir.join(&file_name);
        fs::write(&path, &s.data)?;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
        secrets_map.insert(s.id.clone(), (file_name, s.data.clone()));
    }

    // Ensure the persistent cache-mounts directory exists before booting the builder VM.
    let cache_root = cache_mounts_dir();
    fs::create_dir_all(&cache_root)?;
    let _ = fs::set_permissions(&cache_root, fs::Permissions::from_mode(0o700));

    // --no-cache skips lookups but still records what it builds.
    let mut cache = Cache::load();
    let mut builder: Option<Builder> = None;
    let mut finished: HashMap<usize, (State, Option<String>)> = HashMap::new();
    let mut step = 0;
    let mut staged_seq = 0usize;

    let result = (|| -> Result<State> {
        let mut last = None;
        for &i in &needed {
            let stage = &df.stages[i];
            step += 1;
            let base = expand(&stage.base, &global);
            log(format!("Step {step}/{steps} : FROM {base}{}", stage.name.as_ref().map(|n| format!(" AS {n}")).unwrap_or_default()));
            let mut state = match df.stage_index(&base).filter(|d| *d < i) {
                Some(d) => {
                    let mut s = finished.get(&d).map(|(s, _)| s.clone()).ok_or_else(|| anyhow!("stage {base} wasn't built"))?;
                    s.args.clear();
                    s
                }
                None => base_state(engine, &base, spec.platform.as_deref(), log)?,
            };
            if state.architecture == "amd64" && graviton_image::guest_arch() != "amd64" && !AppleHypervisor::rosetta_available() {
                bail!(
                    "building a linux/amd64 image needs Apple Rosetta for Linux (run `softwareupdate --install-rosetta`)"
                );
            }
            let container = format!("b{build_id}s{i}");
            let mut live = false; // the container holds `state.layers`
            let mut use_cache = !spec.no_cache;

            for ins in &stage.instructions {
                step += 1;
                log(format!("Step {step}/{steps} : {}", ins.text));
                let vars = state.vars();
                let mut prepared_copy: Option<PreparedCopy> = None;

                let fs_step: Option<String> = match &ins.kind {
                    Kind::Run { command, mounts, stdin } => {
                        let args: Vec<String> = state.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
                        let cmd_key = match command {
                            Command::Exec(v) => format!("exec:{}", serde_json::to_string(v).unwrap_or_default()),
                            Command::Shell(s) => format!("shell:{s}"),
                        };
                        let stdin_key = stdin.as_deref().map(|s| hash(&["stdin", s])).unwrap_or_default();
                        let mount_key = run_mounts_cache_key(engine, &df, mounts, &context, &vars, &ignore, &finished)
                            .with_context(|| format!("Dockerfile line {}: {}", ins.line, ins.text))?;
                        Some(format!(
                            "{}|cmd:{cmd_key}|in:{stdin_key}|mnt:{mount_key}|args:{}|wd:{}|u:{}",
                            ins.text,
                            args.join(","),
                            state.workdir,
                            state.user.as_deref().unwrap_or("")
                        ))
                    }
                    Kind::Copy { sources, heredocs, dest, from, chown, chmod, checksum, add } => {
                        let prep = prepare_copy(
                            engine,
                            &df,
                            &context,
                            &staged_dir,
                            &mut staged_seq,
                            sources,
                            heredocs,
                            dest,
                            from.as_deref(),
                            chown.as_deref(),
                            chmod.as_deref(),
                            checksum.as_deref(),
                            *add,
                            &vars,
                            &ignore,
                            &finished,
                            log,
                        )
                        .with_context(|| format!("Dockerfile line {}: {}", ins.line, ins.text))?;
                        let material = format!(
                            "{}|{}|dest:{}|wd:{}|chown:{}|chmod:{:?}",
                            ins.text,
                            prep.source_key,
                            prep.dest,
                            state.workdir,
                            prep.chown.as_deref().unwrap_or(""),
                            prep.chmod
                        );
                        prepared_copy = Some(prep);
                        Some(material)
                    }
                    Kind::Workdir(_) => Some(ins.text.clone()),
                    _ => None,
                };

                // Steps that only change the config.
                let Some(material) = fs_step else {
                    apply_config(&mut state, &ins.kind, &vars, &build_args)?;
                    state.key = hash(&[&state.key, &ins.text]);
                    state.history.push(json!({"created_by": ins.text, "empty_layer": true}));
                    continue;
                };
                let key = hash(&[&state.key, &material]);
                if let Kind::Workdir(dir) = &ins.kind {
                    state.workdir = absolute(&expand(dir, &vars), &state.workdir);
                }

                if use_cache {
                    let present = |s: &CachedStep| s.layer.as_ref().is_none_or(|l| Digest::parse(&l.digest).is_ok_and(|d| engine.blobs.has(&d)));
                    if let Some(hit) = cache.steps.get(&key).cloned().filter(present) {
                        log(" ---> Using cache".into());
                        push_layer(&mut state, hit.layer, &ins.text);
                        state.created = state.created.max(Some(hit.created));
                        state.key = key;
                        continue;
                    }
                    use_cache = false; // a miss: rebuild the rest of the stage
                }

                // Run the step in the builder VM.
                let b = match builder.as_mut() {
                    Some(b) => b,
                    None => builder.insert(boot_builder(engine, &context, &build_dir, &out_dir, &staged_dir, &secrets_dir, &cache_root, log)?),
                };
                if !live {
                    prepare(b, &container, &state.layers)?;
                    live = true;
                }
                b.request(&AgentRequest::Snapshot { container: container.clone() })?;
                match &ins.kind {
                    Kind::Run { command, mounts, stdin } => {
                        let (exec_mounts, extra_env) = resolve_exec_mounts(
                            engine,
                            b,
                            &df,
                            mounts,
                            &vars,
                            &build_id,
                            &mut finished,
                            &secrets_map,
                        )
                        .with_context(|| format!("Dockerfile line {}: {}", ins.line, ins.text))?;
                        run(b, &container, &state, command, stdin.as_deref(), exec_mounts, &extra_env, log)
                            .with_context(|| format!("Dockerfile line {}: {}", ins.line, ins.text))?;
                    }
                    Kind::Copy { from, add, .. } => {
                        let prep = prepared_copy.take().unwrap();
                        let from_container = match from {
                            None => None,
                            Some(f) => Some(source_container(engine, b, &df, f, &build_id, &mut finished)?),
                        };
                        let copied = b.request(&AgentRequest::Copy {
                            container: container.clone(),
                            sources: prep.local_sources,
                            dest: prep.dest,
                            cwd: state.workdir.clone(),
                            from_container,
                            chown: prep.chown,
                            chmod: prep.chmod,
                            extract: *add,
                            ignore: if from.is_some() { vec![] } else { ignore_lines.clone() },
                            inline: prep.inline,
                            staged: prep.staged,
                        });
                        let files = match copied.with_context(|| format!("Dockerfile line {}: {}", ins.line, ins.text))? {
                            AgentEvent::Copied { files } => files,
                            other => bail!("unexpected answer to copy: {other:?}"),
                        };
                        log(format!(" ---> {files} file{}", if files == 1 { "" } else { "s" }));
                    }
                    Kind::Workdir(_) => {
                        b.request(&AgentRequest::Mkdir { container: container.clone(), path: state.workdir.clone() })?;
                    }
                    _ => unreachable!(),
                }
                b.seq += 1;
                let name = format!("{container}-{}.tgz", b.seq);
                let layer = match b.request(&AgentRequest::Commit { container: container.clone(), name })? {
                    AgentEvent::Committed(info) => store_layer(engine, &b.out_dir, info)?,
                    other => bail!("unexpected answer to commit: {other:?}"),
                };
                match &layer {
                    Some(l) => log(format!(" ---> {} ({})", &l.digest[7..19], human(l.size))),
                    None => log(" ---> no file changes".into()),
                }
                let now = now_secs();
                cache.steps.insert(key.clone(), CachedStep { layer: layer.clone(), created: now });
                state.created = Some(now);
                push_layer(&mut state, layer, &ins.text);
                state.key = key;
            }
            finished.insert(i, (state.clone(), live.then_some(container)));
            last = Some(state);
        }
        last.ok_or_else(|| anyhow!("nothing to build"))
    })();

    cache.save();
    if let Some(b) = builder.take() {
        b.shutdown();
    }
    let _ = fs::remove_dir_all(&build_dir);
    let state = result?;
    let mut state = state;
    for (k, v) in &spec.labels {
        state.labels.insert(k.clone(), v.clone());
    }
    let image = write_image(engine, &reference, &state)?;
    engine.index.lock().unwrap().insert(image.clone())?;
    engine.refresh_images();
    log(format!("Built {} ({}, {} layers) in {:.1} s", display(&reference), human(image.size()), image.layers.len(), t0.elapsed().as_secs_f64()));
    let used_by = 0;
    Ok(image.summary(used_by))
}

#[allow(clippy::too_many_arguments)]
fn prepare_copy(
    engine: &Engine,
    df: &Dockerfile,
    context: &Path,
    staged_dir: &Path,
    staged_seq: &mut usize,
    sources: &[String],
    heredocs: &[Heredoc],
    dest: &str,
    from: Option<&str>,
    chown: Option<&str>,
    chmod: Option<&str>,
    checksum: Option<&str>,
    add: bool,
    vars: &BTreeMap<String, String>,
    ignore: &Ignore,
    finished: &HashMap<usize, (State, Option<String>)>,
    log: &mut dyn FnMut(String),
) -> Result<PreparedCopy> {
    let dest = expand(dest, vars);
    let chown = chown.map(|c| expand(c, vars));
    let chmod = match chmod {
        Some(m) => Some(parse_octal_mode(&expand(m, vars)).map_err(|e| anyhow!("{e}"))?),
        None => None,
    };
    let checksum = checksum.map(|c| expand(c, vars));

    let mut local_sources = Vec::new();
    let mut url_sources = Vec::new();
    for s in sources {
        let expanded = expand(s, vars);
        if expanded.starts_with("http://") || expanded.starts_with("https://") {
            if !add {
                bail!("COPY doesn't accept URLs; use ADD <url> instead");
            }
            url_sources.push(expanded);
        } else {
            local_sources.push(expanded);
        }
    }

    if checksum.is_some() && url_sources.len() != 1 {
        bail!("ADD --checksum requires exactly one HTTP(S) URL source");
    }

    let mut key_parts = Vec::new();

    // 1. Local context or --from stage/image sources.
    if !local_sources.is_empty() {
        let h = match from {
            Some(f) => match df.stage_index(f) {
                Some(d) => finished.get(&d).map(|(s, _)| s.key.clone()).ok_or_else(|| anyhow!("COPY --from={f}: that stage comes later"))?,
                None => engine.image_for(f)?.manifest_digest,
            },
            None => content_hash(context, &local_sources, ignore)?,
        };
        key_parts.push(format!("src:{h}"));
    }

    // 2. Inline heredoc files (`COPY <<EOF /dest`).
    let mut inline = Vec::with_capacity(heredocs.len());
    for hd in heredocs {
        let content = if hd.expand { expand(&hd.content, vars) } else { hd.content.clone() };
        key_parts.push(format!("hd:{}:{}", hd.name, hash(&[&hd.name, &content])));
        inline.push(InlineFile { name: hd.name.clone(), content: content.into_bytes() });
    }

    // 3. Remote URLs (`ADD https://... /dest`), downloaded on the host and staged read-only.
    let mut staged = Vec::with_capacity(url_sources.len());
    for url in url_sources {
        let name = url_filename(&url);
        *staged_seq += 1;
        let rel = format!("{}/{name}", *staged_seq);
        let target = staged_dir.join(&rel);
        log(format!(" ---> downloading {url}"));
        let (bytes, digest) = graviton_image::fetch_url(&url, &target).with_context(|| format!("ADD {url}"))?;
        if let Some(expected) = &checksum {
            let norm = expected.trim().to_ascii_lowercase();
            let norm = if norm.starts_with("sha256:") { norm } else { format!("sha256:{norm}") };
            if digest.as_str() != norm {
                let _ = fs::remove_file(&target);
                bail!("ADD --checksum mismatch for {url}: expected {norm}, got {digest}");
            }
        }
        log(format!(" ---> downloaded {name} ({}, {})", human(bytes), digest.short()));
        key_parts.push(format!("url:{name}:{digest}"));
        staged.push(StagedFile { staged_path: rel, name, mode: 0o600 });
    }

    Ok(PreparedCopy {
        local_sources,
        inline,
        staged,
        chmod,
        chown,
        dest,
        source_key: key_parts.join("|"),
    })
}

fn url_filename(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let after_scheme = without_query.split_once("://").map(|(_, r)| r).unwrap_or(without_query);
    let path_part = after_scheme.split_once('/').map(|(_, p)| p).unwrap_or("");
    let candidate = path_part.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download");
    let clean: String = candidate.chars().filter(|&c| c != '/' && c != '\0').collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        "download".into()
    } else {
        clean
    }
}

fn run_mounts_cache_key(
    engine: &Engine,
    df: &Dockerfile,
    mounts: &[RunMount],
    context: &Path,
    vars: &BTreeMap<String, String>,
    ignore: &Ignore,
    finished: &HashMap<usize, (State, Option<String>)>,
) -> Result<String> {
    let mut parts = Vec::new();
    for m in mounts {
        match m {
            RunMount::Cache { target, id, read_only, mode } => {
                let t = expand(target, vars);
                let i = id.as_deref().map(|s| expand(s, vars)).unwrap_or_else(|| t.clone());
                parts.push(format!("cache:{i}:{t}:{read_only}:{}", mode.as_deref().unwrap_or("")));
            }
            RunMount::Secret { id, target, env, required, .. } => {
                // Never hash secret values into the cache key (matches BuildKit).
                parts.push(format!(
                    "secret:{}:{}:{}:{required}",
                    expand(id, vars),
                    target.as_deref().map(|s| expand(s, vars)).unwrap_or_default(),
                    env.as_deref().map(|s| expand(s, vars)).unwrap_or_default()
                ));
            }
            RunMount::Bind { source, target, from, read_write } => {
                let src = expand(source, vars);
                let tgt = expand(target, vars);
                let h = match from {
                    Some(f) => match df.stage_index(f) {
                        Some(d) => finished.get(&d).map(|(s, _)| s.key.clone()).ok_or_else(|| anyhow!("--mount=type=bind,from={f}: that stage comes later"))?,
                        None => engine.image_for(f)?.manifest_digest,
                    },
                    None => content_hash(context, std::slice::from_ref(&src), ignore)?,
                };
                parts.push(format!("bind:{src}:{tgt}:{read_write}:{h}"));
            }
            RunMount::Tmpfs { target } => {
                parts.push(format!("tmpfs:{}", expand(target, vars)));
            }
        }
    }
    Ok(parts.join(";"))
}

#[allow(clippy::too_many_arguments)]
fn resolve_exec_mounts(
    engine: &Engine,
    b: &mut Builder,
    df: &Dockerfile,
    mounts: &[RunMount],
    vars: &BTreeMap<String, String>,
    build_id: &str,
    finished: &mut HashMap<usize, (State, Option<String>)>,
    secrets: &HashMap<String, (String, Vec<u8>)>,
) -> Result<(Vec<ExecMount>, Vec<String>)> {
    let mut out = Vec::new();
    let mut extra_env = Vec::new();

    for m in mounts {
        match m {
            RunMount::Cache { target, id, read_only, mode } => {
                let target = expand(target, vars);
                let key_input = id.as_deref().map(|s| expand(s, vars)).unwrap_or_else(|| target.clone());
                let key = cache_mount_key(&key_input);
                ensure_cache_mount_dir(&key)?;
                let mode = mode.as_deref().map(|s| parse_octal_mode(&expand(s, vars)).map_err(|e| anyhow!("{e}"))).transpose()?;
                out.push(ExecMount {
                    kind: ExecMountKind::Cache,
                    source: key,
                    from_container: None,
                    target,
                    read_only: *read_only,
                    mode,
                });
            }
            RunMount::Secret { id, target, env, required, mode } => {
                let id = expand(id, vars);
                let Some((file_name, raw)) = secrets.get(&id) else {
                    if *required {
                        bail!("RUN requires secret `{id}` (pass `grav build --secret id={id},src=FILE`)");
                    }
                    continue;
                };
                if let Some(env_var) = env {
                    let var_name = expand(env_var, vars);
                    let val = String::from_utf8_lossy(raw);
                    extra_env.push(format!("{var_name}={val}"));
                }
                let target = target.as_deref().map(|t| expand(t, vars)).unwrap_or_else(|| format!("/run/secrets/{id}"));
                let mode = mode.as_deref().map(|s| parse_octal_mode(&expand(s, vars)).map_err(|e| anyhow!("{e}"))).transpose()?;
                out.push(ExecMount {
                    kind: ExecMountKind::Secret,
                    source: file_name.clone(),
                    from_container: None,
                    target,
                    read_only: true,
                    mode,
                });
            }
            RunMount::Bind { source, target, from, read_write } => {
                let source = expand(source, vars);
                let target = expand(target, vars);
                let from_container = match from {
                    None => None,
                    Some(f) => Some(source_container(engine, b, df, f, build_id, finished)?),
                };
                out.push(ExecMount {
                    kind: ExecMountKind::Bind,
                    source,
                    from_container,
                    target,
                    read_only: !*read_write,
                    mode: None,
                });
            }
            RunMount::Tmpfs { target } => {
                out.push(ExecMount {
                    kind: ExecMountKind::Tmpfs,
                    source: String::new(),
                    from_container: None,
                    target: expand(target, vars),
                    read_only: false,
                    mode: None,
                });
            }
        }
    }

    Ok((out, extra_env))
}

fn display(reference: &str) -> String {
    ImageReference::parse(reference).map(|r| r.display_name()).unwrap_or_else(|_| reference.to_string())
}

/// The tag asked for, or the context folder's name.
fn image_reference(spec: &BuildSpec, context: &Path) -> Result<String> {
    let tag = match &spec.tag {
        Some(t) => t.clone(),
        None => {
            let name: String = context
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
                .collect();
            let name = name.trim_matches(['-', '.', '_']).to_string();
            if name.is_empty() { "graviton-build".into() } else { name }
        }
    };
    Ok(ImageReference::parse(&tag).with_context(|| format!("`{tag}` isn't a valid image name"))?.to_string())
}

/// A stage starting from an image (pulled if needed) or `scratch`.
fn base_state(engine: &Engine, base: &str, platform: Option<&str>, log: &mut dyn FnMut(String)) -> Result<State> {
    let target_arch = match platform {
        Some(p) => graviton_image::normalize_platform(p)?,
        None => graviton_image::guest_arch().into(),
    };
    let empty = State {
        architecture: target_arch.clone(),
        layers: vec![],
        base_config: json!({"architecture": target_arch, "os": "linux", "config": {}}),
        env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        entrypoint: None,
        cmd: None,
        workdir: "/".into(),
        user: None,
        shell: vec!["/bin/sh".into(), "-c".into()],
        exposed: BTreeSet::new(),
        labels: BTreeMap::new(),
        volumes: BTreeSet::new(),
        stop_signal: None,
        history: vec![],
        key: hash(&["scratch", &target_arch]),
        args: BTreeMap::new(),
        created: None,
    };
    if base == "scratch" {
        return Ok(empty);
    }
    let need_pull = engine
        .index
        .lock()
        .unwrap()
        .get(base)
        .is_none_or(|i| platform.is_some() && i.architecture != target_arch);
    if need_pull {
        log(format!(
            " ---> pulling {base}{}",
            platform.map(|p| format!(" ({p})")).unwrap_or_default()
        ));
    }
    let image = engine.image_for_platform(base, platform)?;
    let raw = engine.blobs.read(&Digest::parse(&image.config_digest)?)?;
    let config: Value = serde_json::from_slice(&raw).context("reading the base image's config")?;
    let diff_ids: Vec<String> = config["rootfs"]["diff_ids"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    if diff_ids.len() != image.layers.len() {
        bail!("{base}'s config lists {} layers but its manifest has {}", diff_ids.len(), image.layers.len());
    }
    let c = &config["config"];
    let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>());
    Ok(State {
        architecture: image.architecture.clone(),
        layers: image
            .layers
            .iter()
            .zip(&diff_ids)
            .map(|(l, d)| BuiltLayer { digest: l.digest.clone(), diff_id: d.clone(), size: l.size, media_type: l.media_type.clone() })
            .collect(),
        env: strings(&c["Env"]).unwrap_or(empty.env.clone()),
        entrypoint: strings(&c["Entrypoint"]),
        cmd: strings(&c["Cmd"]),
        workdir: c["WorkingDir"].as_str().filter(|w| !w.is_empty()).unwrap_or("/").to_string(),
        user: c["User"].as_str().filter(|u| !u.is_empty()).map(String::from),
        shell: strings(&c["Shell"]).filter(|s| !s.is_empty()).unwrap_or(empty.shell.clone()),
        exposed: c["ExposedPorts"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
        labels: c["Labels"].as_object().map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()).unwrap_or_default(),
        volumes: c["Volumes"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
        stop_signal: c["StopSignal"].as_str().map(String::from),
        history: config["history"].as_array().cloned().unwrap_or_default(),
        key: hash(&["image", &image.manifest_digest, &image.architecture]),
        base_config: config,
        args: BTreeMap::new(),
        created: None,
    })
}

fn absolute(path: &str, cwd: &str) -> String {
    let joined = if path.starts_with('/') { path.to_string() } else { format!("{}/{path}", cwd.trim_end_matches('/')) };
    format!("/{}", graviton_proto::build::components(&joined).join("/"))
}

fn apply_config(s: &mut State, kind: &Kind, vars: &BTreeMap<String, String>, build_args: &BTreeMap<String, String>) -> Result<()> {
    let argv = |c: &Command, shell: &[String]| match c {
        Command::Exec(v) => v.clone(),
        Command::Shell(line) => shell.iter().cloned().chain([line.clone()]).collect(),
    };
    match kind {
        Kind::Env(pairs) => {
            for (k, v) in pairs {
                let value = expand(v, &s.vars());
                s.set_env(k, &value);
            }
        }
        Kind::Arg(name, default) => {
            let value = build_args.get(name).cloned().or_else(|| default.as_ref().map(|d| expand(d, vars)));
            if let Some(v) = value {
                s.args.insert(name.clone(), v);
            }
        }
        Kind::Cmd(c) => s.cmd = Some(argv(c, &s.shell)),
        Kind::Entrypoint(c) => {
            s.entrypoint = Some(argv(c, &s.shell));
            s.cmd = None; // as in Docker: a new ENTRYPOINT resets an inherited CMD
        }
        Kind::Shell(v) => s.shell = v.clone(),
        Kind::User(u) => s.user = Some(expand(u, vars)),
        Kind::Expose(ports) => {
            for p in ports {
                let p = expand(p, vars);
                s.exposed.insert(if p.contains('/') { p } else { format!("{p}/tcp") });
            }
        }
        Kind::Label(pairs) => {
            for (k, v) in pairs {
                s.labels.insert(expand(k, vars), expand(v, vars));
            }
        }
        Kind::Volume(v) => s.volumes.extend(v.iter().map(|p| expand(p, vars))),
        Kind::StopSignal(sig) => s.stop_signal = Some(expand(sig, vars)),
        Kind::Run { .. } | Kind::Copy { .. } | Kind::Workdir(_) => unreachable!(),
    }
    Ok(())
}

fn push_layer(s: &mut State, layer: Option<BuiltLayer>, text: &str) {
    let empty = layer.is_none();
    if let Some(l) = layer {
        s.layers.push(l);
    }
    s.history.push(json!({"created_by": text, "empty_layer": empty}));
}

/// What COPY/bind from the context would take: paths, kinds, modes and contents.
fn content_hash(context: &Path, sources: &[String], ignore: &Ignore) -> Result<String> {
    let mut h = Sha256::new();
    for src in sources {
        let rel = src.trim_start_matches('/');
        for top in expand_source(context, if rel.is_empty() { "." } else { rel }, ignore)? {
            for p in walk(context, &top, ignore)? {
                let abs = context.join(&p);
                let meta = fs::symlink_metadata(&abs)?;
                h.update(p.to_string_lossy().as_bytes());
                h.update(format!("|{:o}|", meta.permissions().mode()).as_bytes());
                if meta.is_file() {
                    let mut f = File::open(&abs)?;
                    let mut buf = vec![0u8; 256 * 1024];
                    loop {
                        let n = f.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        h.update(&buf[..n]);
                    }
                } else if meta.file_type().is_symlink() {
                    h.update(fs::read_link(&abs)?.to_string_lossy().as_bytes());
                }
                h.update([0]);
            }
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[allow(clippy::too_many_arguments)]
fn boot_builder(
    engine: &Engine,
    context: &Path,
    dir: &Path,
    out_dir: &Path,
    staged_dir: &Path,
    secrets_dir: &Path,
    cache_root: &Path,
    log: &mut dyn FnMut(String),
) -> Result<Builder> {
    let t0 = Instant::now();
    let guest = paths::guest_dir();
    let mut config = VmConfig::new(guest.join("vmlinux"), guest.join("initrd.img"), crate::runtime::scratch_disk(dir)?);
    let host_cpus = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
    config.cpus = host_cpus.clamp(1, 4);
    config.memory_mib = 4096;
    config.network = NetworkMode::Nat;
    config.console_log = Some(dir.join("console.log"));
    config.rosetta = AppleHypervisor::rosetta_available();
    config.balloon = true;
    config.shares = vec![
        SharedDirectory { host_path: engine.blobs.root().to_path_buf(), tag: "blobs".into(), read_only: true },
        SharedDirectory { host_path: context.to_path_buf(), tag: "context".into(), read_only: true },
        SharedDirectory { host_path: out_dir.to_path_buf(), tag: "out".into(), read_only: false },
        SharedDirectory { host_path: cache_root.to_path_buf(), tag: "cache".into(), read_only: false },
        SharedDirectory { host_path: staged_dir.to_path_buf(), tag: "staged".into(), read_only: true },
        SharedDirectory { host_path: secrets_dir.to_path_buf(), tag: "secrets".into(), read_only: true },
    ];
    let vm = Arc::new(AppleHypervisor::new().boot_vm(&config).context("booting the builder VM")?);
    let mut conn = File::from(vm.connect(AGENT_VSOCK_PORT).context("connecting to the builder VM")?);
    match read_event_unbuffered(&mut conn)? {
        AgentEvent::Ready { .. } => {}
        other => bail!("expected ready from the builder VM, got {other:?}"),
    }
    let events = Events::new(BufReader::new(conn.try_clone()?));
    log(format!(" ---> builder VM up in {} ms ({} vCPU, 4 GiB)", t0.elapsed().as_millis(), config.cpus));
    Ok(Builder { vm, writer: conn, events, out_dir: out_dir.to_path_buf(), prepared: BTreeSet::new(), seq: 0 })
}

fn prepare(b: &mut Builder, container: &str, layers: &[BuiltLayer]) -> Result<()> {
    match b.request(&AgentRequest::Prepare {
        container: container.into(),
        layers: layers.iter().map(|l| l.digest.clone()).collect(),
        mounts: vec![],
        hostname: Some("graviton-build".into()),
        now_secs: None,
    })? {
        AgentEvent::Prepared { .. } => {
            b.prepared.insert(container.into());
            Ok(())
        }
        other => bail!("unexpected answer to prepare: {other:?}"),
    }
}

/// A container holding `from`'s files: another stage's, or an image's.
fn source_container(
    engine: &Engine,
    b: &mut Builder,
    df: &Dockerfile,
    from: &str,
    build_id: &str,
    finished: &mut HashMap<usize, (State, Option<String>)>,
) -> Result<String> {
    if let Some(d) = df.stage_index(from) {
        let (state, live) = finished.get(&d).ok_or_else(|| anyhow!("COPY --from={from}: that stage comes later"))?;
        if let Some(c) = live {
            return Ok(c.clone());
        }
        let name = format!("b{build_id}x{d}");
        if !b.prepared.contains(&name) {
            let layers = state.layers.clone();
            prepare(b, &name, &layers)?;
        }
        return Ok(name);
    }
    let image = engine.image_for(from)?;
    let name = format!("b{build_id}i{}", &image.manifest_digest[7..19]);
    if !b.prepared.contains(&name) {
        let layers: Vec<BuiltLayer> = image.layers.iter().map(|l| BuiltLayer { digest: l.digest.clone(), diff_id: String::new(), size: l.size, media_type: l.media_type.clone() }).collect();
        prepare(b, &name, &layers)?;
    }
    Ok(name)
}

#[allow(clippy::too_many_arguments)]
fn run(
    b: &mut Builder,
    container: &str,
    s: &State,
    c: &Command,
    stdin: Option<&str>,
    mounts: Vec<ExecMount>,
    extra_env: &[String],
    log: &mut dyn FnMut(String),
) -> Result<()> {
    let argv = match c {
        Command::Exec(v) => v.clone(),
        Command::Shell(line) => s.shell.iter().cloned().chain([line.clone()]).collect(),
    };
    // ARGs are environment variables for RUN only; ENV wins over an ARG of the same name;
    // secret `env=` vars apply to this single RUN only.
    let mut env: Vec<String> = s.args.iter().filter(|(k, _)| !s.env.iter().any(|e| e.starts_with(&format!("{k}=")))).map(|(k, v)| format!("{k}={v}")).collect();
    env.extend(s.env.iter().cloned());
    env.extend(extra_env.iter().cloned());
    b.writer.write_all(&encode_line(&AgentRequest::Exec {
        argv,
        env,
        cwd: s.workdir.clone(),
        tty: false,
        container: Some(container.into()),
        user: s.user.clone(),
        hostname: Some("graviton-build".into()),
        rows: 0,
        cols: 0,
        stdin: stdin.map(|s| s.as_bytes().to_vec()),
        stdin_pipe: false,
        mounts,
    }))?;
    let (mut out, mut err) = (LineBuffer::default(), LineBuffer::default());
    loop {
        match b.events.next()? {
            AgentEvent::Started { .. } => {}
            AgentEvent::Stdout { data, .. } => out.push(&data).into_iter().for_each(&mut *log),
            AgentEvent::Stderr { data, .. } => err.push(&data).into_iter().for_each(&mut *log),
            AgentEvent::Exited { code, .. } => {
                out.finish().into_iter().chain(err.finish()).for_each(&mut *log);
                if code != 0 {
                    bail!("the command returned {code}");
                }
                return Ok(());
            }
            AgentEvent::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}

/// Move a committed layer from the out folder into the blob store.
fn store_layer(engine: &Engine, out_dir: &Path, info: LayerInfo) -> Result<Option<BuiltLayer>> {
    let Some(file) = info.file else { return Ok(None) };
    let path = out_dir.join(&file);
    let digest = Digest::parse(&info.digest)?;
    if !engine.blobs.has(&digest) {
        engine.blobs.write_verified(&digest, File::open(&path)?, |_| {})?;
    }
    let _ = fs::remove_file(&path);
    Ok(Some(BuiltLayer { digest: info.digest, diff_id: info.diff_id, size: info.size, media_type: GZIP_LAYER.into() }))
}

fn write_image(engine: &Engine, reference: &str, s: &State) -> Result<LocalImage> {
    let mut config = s.base_config.clone();
    let obj = |v: BTreeSet<String>| Value::Object(v.into_iter().map(|k| (k, json!({}))).collect());
    let mut rc = serde_json::Map::new();
    rc.insert("Env".into(), json!(s.env));
    if let Some(e) = &s.entrypoint {
        rc.insert("Entrypoint".into(), json!(e));
    }
    if let Some(c) = &s.cmd {
        rc.insert("Cmd".into(), json!(c));
    }
    rc.insert("WorkingDir".into(), json!(s.workdir));
    if let Some(u) = &s.user {
        rc.insert("User".into(), json!(u));
    }
    if !s.exposed.is_empty() {
        rc.insert("ExposedPorts".into(), obj(s.exposed.clone()));
    }
    if !s.labels.is_empty() {
        rc.insert("Labels".into(), json!(s.labels));
    }
    if !s.volumes.is_empty() {
        rc.insert("Volumes".into(), obj(s.volumes.clone()));
    }
    if let Some(sig) = &s.stop_signal {
        rc.insert("StopSignal".into(), json!(sig));
    }
    // The newest step's time; with nothing but config changes, the base image's.
    let created = match s.created {
        Some(t) => rfc3339(t),
        None => s.base_config["created"].as_str().map(String::from).unwrap_or_else(|| rfc3339(0)),
    };
    config["config"] = Value::Object(rc);
    config["architecture"] = json!(s.architecture);
    config["os"] = json!("linux");
    config["created"] = json!(created);
    config["rootfs"] = json!({"type": "layers", "diff_ids": s.layers.iter().map(|l| l.diff_id.clone()).collect::<Vec<_>>()});
    config["history"] = json!(s.history);
    if let Some(map) = config.as_object_mut() {
        map.remove("container_config");
        map.remove("container");
    }
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = Digest::of(&config_bytes);
    engine.blobs.write_bytes(&config_digest, &config_bytes)?;

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest.as_str(), "size": config_bytes.len()},
        "layers": s.layers.iter().map(|l| json!({"mediaType": l.media_type, "digest": l.digest, "size": l.size})).collect::<Vec<_>>(),
        "annotations": {"org.opencontainers.image.created": created, "dev.graviton.built-by": "grav build"},
    });
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = Digest::of(&manifest_bytes);
    engine.blobs.write_bytes(&manifest_digest, &manifest_bytes)?;

    Ok(LocalImage {
        reference: reference.to_string(),
        manifest_digest: manifest_digest.to_string(),
        config_digest: config_digest.to_string(),
        layers: s.layers.iter().map(|l| Layer { digest: l.digest.clone(), size: l.size, media_type: l.media_type.clone() }).collect(),
        architecture: s.architecture.clone(),
        pulled_at: now_secs(),
    })
}

fn human(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Unix seconds as `2026-11-10T20:55:00Z`.
pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_paths() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_794_304_448), "2026-11-10T09:54:08Z");
        assert_eq!(absolute("app", "/"), "/app");
        assert_eq!(absolute("../x", "/srv/app"), "/srv/x");
        assert_eq!(absolute("/opt/./y/", "/srv"), "/opt/y");
        assert_eq!(url_filename("https://example.com/a/b/tool.tar.gz?sig=1"), "tool.tar.gz");
        assert_eq!(url_filename("https://example.com/"), "download");
        assert_eq!(cache_mount_key("/root/.cache/pip"), cache_mount_key("/root/.cache/pip"));
        assert_ne!(cache_mount_key("/root/.cache/pip"), cache_mount_key("/var/cache/apk"));
    }

    #[test]
    fn names_images_after_the_folder() {
        let spec = |tag: Option<&str>| BuildSpec {
            context: "/x".into(),
            dockerfile: None,
            tag: tag.map(String::from),
            build_args: vec![],
            target: None,
            no_cache: false,
            upload: None,
            secrets: vec![],
            platform: None,
            labels: BTreeMap::new(),
        };
        assert_eq!(image_reference(&spec(None), Path::new("/Users/me/My App")).unwrap(), "docker.io/library/my-app:latest");
        assert_eq!(image_reference(&spec(Some("web:1.2")), Path::new("/x")).unwrap(), "docker.io/library/web:1.2");
        assert!(image_reference(&spec(Some("Bad Name")), Path::new("/x")).is_err());
    }

    #[test]
    fn config_steps() {
        let mut s = base_state_for_test();
        let vars = s.vars();
        apply_config(&mut s, &Kind::Env(vec![("A".into(), "1".into()), ("B".into(), "${A}2".into())]), &vars, &BTreeMap::new()).unwrap();
        assert!(s.env.contains(&"B=12".to_string()));
        let v = s.vars();
        apply_config(&mut s, &Kind::Arg("V".into(), Some("d".into())), &v, &BTreeMap::from([("V".to_string(), "cli".to_string())])).unwrap();
        assert_eq!(s.args["V"], "cli");
        let v = s.vars();
        apply_config(&mut s, &Kind::Cmd(Command::Shell("run it".into())), &v, &BTreeMap::new()).unwrap();
        assert_eq!(s.cmd.as_deref(), Some(&["/bin/sh".to_string(), "-c".into(), "run it".into()][..]));
        let v = s.vars();
        apply_config(&mut s, &Kind::Expose(vec!["80".into(), "53/udp".into()]), &v, &BTreeMap::new()).unwrap();
        assert_eq!(s.exposed.iter().cloned().collect::<Vec<_>>(), ["53/udp", "80/tcp"]);
    }

    fn base_state_for_test() -> State {
        State {
            architecture: "arm64".into(),
            layers: vec![],
            base_config: json!({}),
            env: vec!["PATH=/bin".into()],
            entrypoint: None,
            cmd: None,
            workdir: "/".into(),
            user: None,
            shell: vec!["/bin/sh".into(), "-c".into()],
            exposed: BTreeSet::new(),
            labels: BTreeMap::new(),
            volumes: BTreeSet::new(),
            stop_signal: None,
            history: vec![],
            key: String::new(),
            args: BTreeMap::new(),
            created: None,
        }
    }
}
