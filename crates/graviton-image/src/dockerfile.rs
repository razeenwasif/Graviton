//! A Dockerfile parser for `grav build`.
//!
//! Supported: FROM (with `AS name` and earlier stages as bases), RUN (with
//! `--mount=type=cache|secret|bind|tmpfs`, heredocs and `#!` shebangs), CMD,
//! ENTRYPOINT and SHELL in shell or JSON form, COPY and ADD (local files,
//! `ADD <http(s)://url>`, heredocs, `--from`, `--chown`, `--chmod`,
//! `--checksum`), ENV, ARG, WORKDIR, USER, EXPOSE, LABEL, VOLUME, STOPSIGNAL,
//! and line continuations.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `["prog", "arg"]`
    Exec(Vec<String>),
    /// Run through the stage's SHELL (`/bin/sh -c` by default).
    Shell(String),
}

/// An inline heredoc attached to `COPY` / `ADD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heredoc {
    /// Delimiter word (also the default filename when copying into a directory).
    pub name: String,
    pub content: String,
    /// True when the delimiter was unquoted (`<<EOF`), meaning `$VAR` expands.
    pub expand: bool,
}

/// One `--mount=...` attached to a `RUN` instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMount {
    Cache {
        target: String,
        id: Option<String>,
        read_only: bool,
        mode: Option<String>,
    },
    Secret {
        id: String,
        target: Option<String>,
        env: Option<String>,
        required: bool,
        mode: Option<String>,
    },
    Bind {
        source: String,
        target: String,
        from: Option<String>,
        read_write: bool,
    },
    Tmpfs {
        target: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Run {
        command: Command,
        mounts: Vec<RunMount>,
        /// Script fed on stdin when `RUN <<EOF` uses a `#!` shebang.
        stdin: Option<String>,
    },
    Cmd(Command),
    Entrypoint(Command),
    Shell(Vec<String>),
    Copy {
        sources: Vec<String>,
        heredocs: Vec<Heredoc>,
        dest: String,
        from: Option<String>,
        chown: Option<String>,
        chmod: Option<String>,
        checksum: Option<String>,
        add: bool,
    },
    Env(Vec<(String, String)>),
    Arg(String, Option<String>),
    Workdir(String),
    User(String),
    Expose(Vec<String>),
    Label(Vec<(String, String)>),
    Volume(Vec<String>),
    StopSignal(String),
    Healthcheck(Option<graviton_proto::Healthcheck>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// 1-based line where it starts.
    pub line: usize,
    /// One-line summary as written (continuations joined), for the build log.
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub line: usize,
    /// The image or earlier stage it starts from (before ARG expansion).
    pub base: String,
    pub name: Option<String>,
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dockerfile {
    /// ARGs before the first FROM: usable in FROM lines.
    pub global_args: Vec<(String, Option<String>)>,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dockerfile line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse an octal file mode such as `755`, `0644`, or `4755`.
pub fn parse_octal_mode(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(format!("invalid octal mode `{s}` (expected e.g. 755 or 0644)"));
    }
    u32::from_str_radix(s, 8).map_err(|_| format!("invalid octal mode `{s}`"))
}

impl Dockerfile {
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut df = Dockerfile::default();
        for LogicalLine { line, text: raw, heredocs } in logical_lines(text)? {
            let err = |message: String| ParseError { line, message };
            let trimmed = raw.trim();
            let (word, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
            let rest = rest.trim();
            let instr = word.to_ascii_uppercase();
            if instr == "FROM" {
                let mut words = split_words(rest);
                words.retain(|w| !w.starts_with("--platform="));
                let (base, name) = match words.as_slice() {
                    [base] => (base.clone(), None),
                    [base, as_, name] if as_.eq_ignore_ascii_case("as") => (base.clone(), Some(name.to_ascii_lowercase())),
                    _ => return Err(err(format!("FROM needs an image, optionally `AS name`; got `{rest}`"))),
                };
                df.stages.push(Stage { line, base, name, instructions: vec![] });
                continue;
            }
            let kind = match instr.as_str() {
                "RUN" => {
                    let mut rest = rest;
                    let mut mounts = Vec::new();
                    while let Some(flag) = rest.strip_prefix("--") {
                        let (f, after) = flag.split_once(char::is_whitespace).unwrap_or((flag, ""));
                        if let Some(spec) = f.strip_prefix("mount=") {
                            mounts.push(parse_run_mount(spec).map_err(err)?);
                        } else if !f.starts_with("network=") && !f.starts_with("security=") {
                            return Err(err(format!("RUN --{f} isn't supported")));
                        }
                        rest = after.trim_start();
                    }
                    let (cmd, stdin) = build_run_command(rest, &heredocs);
                    Kind::Run { command: cmd, mounts, stdin }
                }
                "CMD" => Kind::Cmd(command(rest)),
                "ENTRYPOINT" => Kind::Entrypoint(command(rest)),
                "SHELL" => match json_array(rest) {
                    Some(v) if !v.is_empty() => Kind::Shell(v),
                    _ => return Err(err("SHELL needs JSON form, e.g. [\"/bin/bash\", \"-c\"]".into())),
                },
                "COPY" | "ADD" => {
                    let add = instr == "ADD";
                    let mut from = None;
                    let mut chown = None;
                    let mut chmod = None;
                    let mut checksum = None;
                    let mut rest = rest;
                    while rest.starts_with("--") {
                        let (flag, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                        if let Some(v) = flag.strip_prefix("--from=") {
                            from = Some(unquote(v));
                        } else if let Some(v) = flag.strip_prefix("--chown=") {
                            chown = Some(unquote(v));
                        } else if let Some(v) = flag.strip_prefix("--chmod=") {
                            let val = unquote(v);
                            if !val.contains('$') {
                                parse_octal_mode(&val).map_err(err)?;
                            }
                            chmod = Some(val);
                        } else if add && flag.starts_with("--checksum=") {
                            checksum = Some(unquote(flag.strip_prefix("--checksum=").unwrap()));
                        } else if flag != "--link" && !flag.starts_with("--link=") && flag != "--keep-git-dir" && !flag.starts_with("--keep-git-dir=") {
                            return Err(err(format!("{instr} {flag} isn't supported")));
                        }
                        rest = after.trim_start();
                    }
                    let mut paths = json_array(rest).unwrap_or_else(|| split_words_keeping_heredoc_tokens(rest));
                    if paths.len() < 2 {
                        return Err(err(format!("{instr} needs at least one source and a destination")));
                    }
                    let dest = paths.pop().unwrap();
                    let mut sources = Vec::new();
                    let mut attached_heredocs = Vec::new();
                    let mut hd_iter = heredocs.into_iter();
                    for p in paths {
                        if parse_heredoc_marker(&p, true).is_some() {
                            let hd = hd_iter
                                .next()
                                .ok_or_else(|| err(format!("{instr} heredoc `{p}` had no body")))?;
                            attached_heredocs.push(Heredoc {
                                name: hd.delimiter,
                                content: hd.content,
                                expand: hd.expand,
                            });
                        } else {
                            if p.contains("://") {
                                if !add {
                                    return Err(err("COPY doesn't accept URLs; use ADD <url> instead".into()));
                                }
                                if !p.starts_with("http://") && !p.starts_with("https://") {
                                    return Err(err(format!("ADD only supports http:// and https:// URLs; got `{p}`")));
                                }
                            }
                            sources.push(p);
                        }
                    }
                    if sources.is_empty() && attached_heredocs.is_empty() {
                        return Err(err(format!("{instr} needs at least one source")));
                    }
                    Kind::Copy { sources, heredocs: attached_heredocs, dest, from, chown, chmod, checksum, add }
                }
                "ENV" => Kind::Env(pairs(rest, true).map_err(err)?),
                "LABEL" => Kind::Label(pairs(rest, false).map_err(err)?),
                "ARG" => {
                    let (name, default) = match rest.split_once('=') {
                        Some((n, d)) => (n.trim().to_string(), Some(unquote(d.trim()))),
                        None => (rest.to_string(), None),
                    };
                    if name.is_empty() || name.contains(char::is_whitespace) {
                        return Err(err(format!("ARG takes one name, optionally =default; got `{rest}`")));
                    }
                    if df.stages.is_empty() {
                        df.global_args.push((name, default));
                        continue;
                    }
                    Kind::Arg(name, default)
                }
                "WORKDIR" => Kind::Workdir(unquote(rest)),
                "USER" => Kind::User(rest.to_string()),
                "EXPOSE" => Kind::Expose(split_words(rest)),
                "VOLUME" => Kind::Volume(json_array(rest).unwrap_or_else(|| split_words(rest))),
                "STOPSIGNAL" => Kind::StopSignal(rest.to_string()),
                "MAINTAINER" => Kind::Label(vec![("maintainer".into(), rest.to_string())]),
                "HEALTHCHECK" => Kind::Healthcheck(parse_healthcheck(rest).map_err(err)?),
                "ONBUILD" => return Err(err("ONBUILD isn't supported".into())),
                other => return Err(err(format!("unknown instruction `{other}`"))),
            };
            let Some(stage) = df.stages.last_mut() else {
                return Err(err(format!("{instr} before the first FROM")));
            };
            stage.instructions.push(Instruction { line, text: trimmed.to_string(), kind });
        }
        if df.stages.is_empty() {
            return Err(ParseError { line: 1, message: "no FROM instruction".into() });
        }
        Ok(df)
    }

    /// The stage named or numbered `name`.
    pub fn stage_index(&self, name: &str) -> Option<usize> {
        let lower = name.to_ascii_lowercase();
        self.stages.iter().position(|s| s.name.as_deref() == Some(lower.as_str())).or_else(|| name.parse().ok().filter(|i| *i < self.stages.len()))
    }
}

fn build_run_command(rest: &str, heredocs: &[RawHeredoc]) -> (Command, Option<String>) {
    if heredocs.is_empty() {
        return (command(rest), None);
    }
    // Pure BuildKit heredoc script (`RUN <<EOF` or `RUN <<-EOF` or `RUN <<'EOF'`).
    if heredocs.len() == 1 && parse_heredoc_marker(rest.trim(), false).is_some() {
        let body = &heredocs[0].content;
        if let Some(first_line) = body.lines().next().and_then(|l| l.strip_prefix("#!")) {
            let mut argv = split_words(first_line.trim());
            if !argv.is_empty() {
                // Mirror kernel shebang execution: pass the script path (backed by an
                // unlinked seekable temp file on fd 0 in the guest) as the final arg.
                argv.push("/proc/self/fd/0".into());
                return (Command::Exec(argv), Some(body.clone()));
            }
        }
        return (Command::Shell(body.clone()), None);
    }
    // Inline shell heredoc(s) such as `RUN cat <<EOF > /etc/app.conf`.
    let mut script = String::from(rest);
    script.push('\n');
    for hd in heredocs {
        script.push_str(&hd.raw_body);
        script.push_str(&hd.delimiter);
        script.push('\n');
    }
    (Command::Shell(script), None)
}

fn parse_healthcheck(raw: &str) -> Result<Option<graviton_proto::Healthcheck>, String> {
    let mut rest = raw.trim();
    if rest.is_empty() {
        return Err("HEALTHCHECK needs NONE or [OPTIONS] CMD <command>".into());
    }
    if rest.eq_ignore_ascii_case("NONE") {
        return Ok(None);
    }

    let mut hc = graviton_proto::Healthcheck::default();
    let mut saw_flags = false;
    while rest.starts_with("--") {
        let (flag, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let Some((opt, raw_val)) = flag.split_once('=') else {
            return Err(format!("HEALTHCHECK option `{flag}` requires a `=value`"));
        };
        let val = unquote(raw_val);
        match opt.to_ascii_lowercase().as_str() {
            "--interval" => {
                let secs = graviton_proto::parse_duration_secs(&val)
                    .map_err(|e| format!("invalid HEALTHCHECK --interval: {e}"))?;
                if secs == 0 {
                    return Err("HEALTHCHECK --interval must be greater than 0s".into());
                }
                hc.interval_secs = secs;
            }
            "--timeout" => {
                let secs = graviton_proto::parse_duration_secs(&val)
                    .map_err(|e| format!("invalid HEALTHCHECK --timeout: {e}"))?;
                if secs == 0 {
                    return Err("HEALTHCHECK --timeout must be greater than 0s".into());
                }
                hc.timeout_secs = secs;
            }
            "--start-period" => {
                hc.start_period_secs = graviton_proto::parse_duration_secs(&val)
                    .map_err(|e| format!("invalid HEALTHCHECK --start-period: {e}"))?;
            }
            "--start-interval" => {
                graviton_proto::parse_duration_secs(&val)
                    .map_err(|e| format!("invalid HEALTHCHECK --start-interval: {e}"))?;
            }
            "--retries" => {
                let n: u32 = val.trim().parse().map_err(|_| format!("invalid HEALTHCHECK --retries `{val}`"))?;
                if n == 0 {
                    return Err("HEALTHCHECK --retries must be at least 1".into());
                }
                hc.retries = n;
            }
            _ => return Err(format!("unsupported HEALTHCHECK option `{flag}`")),
        }
        saw_flags = true;
        rest = after.trim_start();
    }

    let (kw, cmd_rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if kw.eq_ignore_ascii_case("NONE") {
        if saw_flags || !cmd_rest.trim().is_empty() {
            return Err("HEALTHCHECK NONE takes no options or arguments".into());
        }
        return Ok(None);
    }
    if !kw.eq_ignore_ascii_case("CMD") {
        return Err(format!("HEALTHCHECK expected CMD or NONE, got `{rest}`"));
    }
    let cmd_rest = cmd_rest.trim();
    if cmd_rest.is_empty() {
        return Err("HEALTHCHECK CMD needs a command".into());
    }
    hc.test = match json_array(cmd_rest) {
        Some(v) => {
            if v.is_empty() {
                return Err("HEALTHCHECK CMD [] cannot be empty".into());
            }
            std::iter::once("CMD".to_string()).chain(v).collect()
        }
        None => vec!["CMD-SHELL".into(), cmd_rest.to_string()],
    };
    Ok(Some(hc))
}

fn parse_run_mount(spec: &str) -> Result<RunMount, String> {
    let mut kind = "bind".to_string();
    let mut target: Option<String> = None;
    let mut source: Option<String> = None;
    let mut id: Option<String> = None;
    let mut from: Option<String> = None;
    let mut env: Option<String> = None;
    let mut mode: Option<String> = None;
    let mut ro: Option<bool> = None;
    let mut rw: Option<bool> = None;
    let mut required = false;

    for item in split_csv(spec) {
        if item.is_empty() {
            continue;
        }
        let (k, v) = match item.split_once('=') {
            Some((k, v)) => (k.trim(), Some(unquote(v.trim()))),
            None => (item.trim(), None),
        };
        let bool_val = |v: Option<String>| -> Result<bool, String> {
            match v.as_deref() {
                None | Some("true") | Some("1") | Some("") => Ok(true),
                Some("false") | Some("0") => Ok(false),
                Some(other) => Err(format!("invalid boolean `{other}` in --mount={spec}")),
            }
        };
        match k {
            "type" => kind = v.ok_or_else(|| format!("mount type needs a value in --mount={spec}"))?,
            "target" | "dst" | "destination" => target = v,
            "source" | "src" => source = v,
            "id" => id = v,
            "from" => from = v,
            "env" => env = v,
            "mode" => {
                if let Some(m) = v {
                    if !m.contains('$') {
                        parse_octal_mode(&m)?;
                    }
                    mode = Some(m);
                }
            }
            "ro" | "readonly" => ro = Some(bool_val(v)?),
            "rw" | "readwrite" => rw = Some(bool_val(v)?),
            "required" => required = bool_val(v)?,
            // Accepted for compatibility with BuildKit Dockerfiles.
            "sharing" | "uid" | "gid" | "size" | "z" | "Z" => {}
            other => return Err(format!("unknown option `{other}` in --mount={spec}")),
        }
    }

    match kind.as_str() {
        "cache" => {
            let target = target.ok_or_else(|| format!("--mount=type=cache needs target=... (got `{spec}`)"))?;
            let read_only = ro.unwrap_or_else(|| rw.map(|w| !w).unwrap_or(false));
            Ok(RunMount::Cache { target, id, read_only, mode })
        }
        "secret" => {
            let id = id
                .or_else(|| {
                    target
                        .as_deref()
                        .and_then(|t| t.rsplit('/').next())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                })
                .ok_or_else(|| format!("--mount=type=secret needs id=... (got `{spec}`)"))?;
            Ok(RunMount::Secret { id, target, env, required, mode })
        }
        "bind" => {
            let target = target.ok_or_else(|| format!("--mount=type=bind needs target=... (got `{spec}`)"))?;
            let source = source.unwrap_or_else(|| ".".into());
            let read_write = rw.unwrap_or_else(|| ro.map(|r| !r).unwrap_or(false));
            Ok(RunMount::Bind { source, target, from, read_write })
        }
        "tmpfs" => {
            let target = target.ok_or_else(|| format!("--mount=type=tmpfs needs target=... (got `{spec}`)"))?;
            Ok(RunMount::Tmpfs { target })
        }
        other => Err(format!("RUN --mount=type={other} isn't supported (use cache, secret, bind, or tmpfs)")),
    }
}

fn split_csv(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, ',') => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Debug, Clone)]
struct RawHeredoc {
    delimiter: String,
    /// Processed content (with leading tabs stripped when `<<-` was used).
    content: String,
    /// Raw lines as written (for inline shell heredocs passed to `/bin/sh -c`).
    raw_body: String,
    expand: bool,
}

#[derive(Debug, Clone)]
struct LogicalLine {
    line: usize,
    text: String,
    heredocs: Vec<RawHeredoc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeredocMarker {
    delimiter: String,
    chomp_tabs: bool,
    expand: bool,
}

/// Join continuation lines (`\` at the end), drop comments/blanks, and attach
/// any heredoc bodies belonging to `RUN`, `COPY`, or `ADD` instructions.
fn logical_lines(text: &str) -> Result<Vec<LogicalLine>, ParseError> {
    let raw_lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut idx = 0usize;

    while idx < raw_lines.len() {
        let mut current: Option<(usize, String)> = None;
        while idx < raw_lines.len() {
            let line = raw_lines[idx];
            let line_no = idx + 1;
            idx += 1;

            let t = line.trim();
            if t.starts_with('#') || (t.is_empty() && current.is_none()) {
                continue; // comments may sit inside a continued instruction
            }
            let (content, continued) = match line.trim_end().strip_suffix('\\') {
                Some(c) => (c.to_string(), true),
                None => (line.to_string(), false),
            };
            match current.as_mut() {
                Some((_, acc)) => {
                    acc.push(' ');
                    acc.push_str(content.trim());
                }
                None => current = Some((line_no, content.trim().to_string())),
            }
            if !continued {
                break;
            }
        }
        let Some((start_line, joined)) = current.filter(|(_, s)| !s.trim().is_empty()) else {
            continue;
        };

        let (word, rest) = joined.trim().split_once(char::is_whitespace).unwrap_or((joined.trim(), ""));
        let instr = word.to_ascii_uppercase();
        let mut heredocs = Vec::new();
        if matches!(instr.as_str(), "RUN" | "COPY" | "ADD") {
            let allow_filenames = instr != "RUN";
            let markers = find_heredoc_markers(rest, allow_filenames);
            for m in markers {
                let mut content = String::new();
                let mut raw_body = String::new();
                let mut closed = false;
                while idx < raw_lines.len() {
                    let body_line = raw_lines[idx].strip_suffix('\r').unwrap_or(raw_lines[idx]);
                    idx += 1;
                    let candidate = if m.chomp_tabs { body_line.trim_start_matches('\t') } else { body_line };
                    if candidate == m.delimiter {
                        closed = true;
                        break;
                    }
                    raw_body.push_str(body_line);
                    raw_body.push('\n');
                    content.push_str(candidate);
                    content.push('\n');
                }
                if !closed {
                    return Err(ParseError {
                        line: start_line,
                        message: format!("unterminated heredoc `<<{}`", m.delimiter),
                    });
                }
                heredocs.push(RawHeredoc { delimiter: m.delimiter, content, raw_body, expand: m.expand });
            }
        }
        out.push(LogicalLine { line: start_line, text: joined, heredocs });
    }
    Ok(out)
}

/// Find heredoc markers (`<<EOF`, `<<-EOF`, `<<'EOF'`, `<<"EOF"`) in `rest`,
/// ignoring quoted strings, `<<<` here-strings, and arithmetic bit-shifts (`1 << 2`).
fn find_heredoc_markers(rest: &str, allow_filenames: bool) -> Vec<HeredocMarker> {
    let mut out = Vec::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        match (quote, b) {
            (Some(q), c) if c == q => {
                quote = None;
                i += 1;
            }
            (Some(b'"'), b'\\') if i + 1 < bytes.len() => i += 2,
            (Some(_), _) => i += 1,
            (None, b'\'' | b'"') => {
                quote = Some(b);
                i += 1;
            }
            (None, b'\\') if i + 1 < bytes.len() => i += 2,
            (None, b'<') if bytes.get(i + 1) == Some(&b'<') => {
                let prev_ok = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'&' | b'|' | b'(');
                let not_herestring = bytes.get(i + 2) != Some(&b'<');
                if prev_ok && not_herestring {
                    // Extract the token starting at `i` up to whitespace or shell separator.
                    let start = i;
                    let mut j = i + 2;
                    if bytes.get(j) == Some(&b'-') {
                        j += 1;
                    }
                    if let Some(&q @ (b'\'' | b'"')) = bytes.get(j) {
                        j += 1;
                        while j < bytes.len() && bytes[j] != q {
                            j += 1;
                        }
                        if j < bytes.len() {
                            j += 1;
                        }
                    } else {
                        while j < bytes.len() && !matches!(bytes[j], b' ' | b'\t' | b';' | b'&' | b'|' | b'>' | b'<' | b')' | b'(') {
                            j += 1;
                        }
                    }
                    let token = &rest[start..j];
                    if let Some(m) = parse_heredoc_marker(token, allow_filenames) {
                        out.push(m);
                        i = j;
                        continue;
                    }
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

fn parse_heredoc_marker(token: &str, allow_filenames: bool) -> Option<HeredocMarker> {
    let rest = token.strip_prefix("<<")?;
    if rest.starts_with('<') {
        return None;
    }
    let (chomp_tabs, rest) = match rest.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (expand, delim) = if let Some(inner) = rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        (false, inner)
    } else if let Some(inner) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        (false, inner)
    } else {
        (true, rest)
    };
    if delim.is_empty() {
        return None;
    }
    let valid = if allow_filenames {
        delim.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) && !delim.starts_with('.')
    } else {
        let first = delim.as_bytes()[0];
        (first.is_ascii_alphabetic() || first == b'_') && delim.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    valid.then(|| HeredocMarker { delimiter: delim.to_string(), chomp_tabs, expand })
}

/// Like `split_words`, except `<<'EOF'` / `<<"EOF"` tokens keep their quotes so
/// COPY/ADD can distinguish heredoc markers from ordinary filenames.
fn split_words_keeping_heredoc_tokens(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    let mut preserve_quotes = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if quote.is_none() && !in_word && c == '<' && chars.peek() == Some(&'<') {
            preserve_quotes = true;
        }
        match (quote, c) {
            (Some(q), c) if c == q => {
                if preserve_quotes {
                    cur.push(c);
                }
                quote = None;
            }
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                if preserve_quotes {
                    cur.push(c);
                }
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                    preserve_quotes = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn json_array(s: &str) -> Option<Vec<String>> {
    s.starts_with('[').then(|| serde_json::from_str::<Vec<String>>(s).ok()).flatten()
}

fn command(rest: &str) -> Command {
    match json_array(rest) {
        Some(v) => Command::Exec(v),
        None => Command::Shell(rest.to_string()),
    }
}

/// Split on whitespace, honouring quotes and backslashes (quotes removed).
pub fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

fn unquote(s: &str) -> String {
    split_words(s).join(" ")
}

/// `k=v k2="v 2"`, or the legacy `k v with spaces` (ENV only).
fn pairs(rest: &str, legacy: bool) -> Result<Vec<(String, String)>, String> {
    let words = split_words(rest);
    if words.first().is_some_and(|w| !w.contains('=')) {
        if !legacy {
            return Err(format!("expected key=value, got `{rest}`"));
        }
        let (k, v) = rest.split_once(char::is_whitespace).ok_or_else(|| format!("ENV needs a value: `{rest}`"))?;
        return Ok(vec![(k.to_string(), unquote(v.trim()))]);
    }
    words
        .into_iter()
        .map(|w| w.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected key=value, got `{w}`")))
        .collect()
}

/// Substitute `$VAR`, `${VAR}`, `${VAR:-default}` and `${VAR:+alt}`; `\$` is a literal `$`.
pub fn expand(s: &str, vars: &BTreeMap<String, String>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && chars.get(i + 1) == Some(&'$') {
            out.push('$');
            i += 2;
        } else if c == '$' && chars.get(i + 1) == Some(&'{') {
            let Some(end) = chars[i + 2..].iter().position(|&c| c == '}').map(|p| p + i + 2) else {
                out.push(c);
                i += 1;
                continue;
            };
            let inner: String = chars[i + 2..end].iter().collect();
            let value = if let Some((name, default)) = inner.split_once(":-") {
                vars.get(name).filter(|v| !v.is_empty()).cloned().unwrap_or_else(|| expand(default, vars))
            } else if let Some((name, alt)) = inner.split_once(":+") {
                if vars.get(name).is_some_and(|v| !v.is_empty()) { expand(alt, vars) } else { String::new() }
            } else {
                vars.get(&inner).cloned().unwrap_or_default()
            };
            out.push_str(&value);
            i = end + 1;
        } else if c == '$' && chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let name: String = chars[start..end].iter().collect();
            out.push_str(vars.get(&name).map_or("", String::as_str));
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_healthcheck_variants_and_rejects_bad_flags() {
        let shell = Dockerfile::parse("FROM alpine\nHEALTHCHECK --interval=5s --timeout=2s --start-period=10s --retries=4 CMD curl -f http://localhost/ || exit 1\n").unwrap();
        assert_eq!(
            shell.stages[0].instructions[0].kind,
            Kind::Healthcheck(Some(graviton_proto::Healthcheck {
                test: vec!["CMD-SHELL".into(), "curl -f http://localhost/ || exit 1".into()],
                interval_secs: 5,
                timeout_secs: 2,
                start_period_secs: 10,
                retries: 4,
            }))
        );

        let exec = Dockerfile::parse("FROM alpine\nHEALTHCHECK --start-interval=1s CMD [\"/bin/true\"]\n").unwrap();
        assert_eq!(
            exec.stages[0].instructions[0].kind,
            Kind::Healthcheck(Some(graviton_proto::Healthcheck {
                test: vec!["CMD".into(), "/bin/true".into()],
                interval_secs: 30,
                timeout_secs: 30,
                start_period_secs: 0,
                retries: 3,
            }))
        );

        let disabled = Dockerfile::parse("FROM alpine\nHEALTHCHECK NONE\n").unwrap();
        assert_eq!(disabled.stages[0].instructions[0].kind, Kind::Healthcheck(None));

        for bad in [
            "FROM alpine\nHEALTHCHECK",
            "FROM alpine\nHEALTHCHECK --retries=0 CMD true",
            "FROM alpine\nHEALTHCHECK --interval=0s CMD true",
            "FROM alpine\nHEALTHCHECK --unknown=1 CMD true",
            "FROM alpine\nHEALTHCHECK --interval=5s NONE",
            "FROM alpine\nHEALTHCHECK CMD []",
        ] {
            assert!(Dockerfile::parse(bad).is_err(), "expected error for: {bad}");
        }
    }

    const SAMPLE: &str = r#"
# syntax=docker/dockerfile:1
ARG PY=3.13
FROM python:${PY}-slim AS build
WORKDIR /src
ENV PIP_NO_CACHE_DIR=1 \
    APP_HOME="/srv/my app"
# a comment inside nothing
RUN --mount=type=cache,target=/root/.cache \
    pip install --upgrade pip && \
    # comments inside continuations are dropped
    echo done
COPY --chown=app:app --chmod=0644 requirements.txt ./
COPY ["src dir/", "/src/"]

FROM alpine:3.22
LABEL org.opencontainers.image.title="demo" version=1
COPY --from=build /src /app
EXPOSE 8000/tcp 9000
USER 1000:1000
ENTRYPOINT ["python3"]
CMD python3 -m http.server
"#;

    #[test]
    fn parses_a_multi_stage_file() {
        let df = Dockerfile::parse(SAMPLE).unwrap();
        assert_eq!(df.global_args, [("PY".to_string(), Some("3.13".to_string()))]);
        assert_eq!(df.stages.len(), 2);
        let build = &df.stages[0];
        assert_eq!((build.base.as_str(), build.name.as_deref()), ("python:${PY}-slim", Some("build")));
        assert_eq!(build.instructions[0].kind, Kind::Workdir("/src".into()));
        assert_eq!(build.instructions[1].kind, Kind::Env(vec![("PIP_NO_CACHE_DIR".into(), "1".into()), ("APP_HOME".into(), "/srv/my app".into())]));
        assert_eq!(
            build.instructions[2].kind,
            Kind::Run {
                command: Command::Shell("pip install --upgrade pip && echo done".into()),
                mounts: vec![RunMount::Cache { target: "/root/.cache".into(), id: None, read_only: false, mode: None }],
                stdin: None,
            }
        );
        assert_eq!(build.instructions[2].line, 9);
        assert_eq!(
            build.instructions[3].kind,
            Kind::Copy {
                sources: vec!["requirements.txt".into()],
                heredocs: vec![],
                dest: "./".into(),
                from: None,
                chown: Some("app:app".into()),
                chmod: Some("0644".into()),
                checksum: None,
                add: false,
            }
        );
        assert_eq!(
            build.instructions[4].kind,
            Kind::Copy {
                sources: vec!["src dir/".into()],
                heredocs: vec![],
                dest: "/src/".into(),
                from: None,
                chown: None,
                chmod: None,
                checksum: None,
                add: false,
            }
        );
        let fin = &df.stages[1];
        assert_eq!(fin.instructions[0].kind, Kind::Label(vec![("org.opencontainers.image.title".into(), "demo".into()), ("version".into(), "1".into())]));
        assert!(matches!(&fin.instructions[1].kind, Kind::Copy { from: Some(f), .. } if f == "build"));
        assert_eq!(fin.instructions[2].kind, Kind::Expose(vec!["8000/tcp".into(), "9000".into()]));
        assert_eq!(fin.instructions[4].kind, Kind::Entrypoint(Command::Exec(vec!["python3".into()])));
        assert_eq!(fin.instructions[5].kind, Kind::Cmd(Command::Shell("python3 -m http.server".into())));
        assert_eq!(df.stage_index("BUILD"), Some(0));
        assert_eq!(df.stage_index("1"), Some(1));
    }

    #[test]
    fn parses_heredocs_mounts_urls_and_chmod() {
        let raw = r#"
FROM alpine
RUN --mount=type=cache,id=apk,target=/var/cache/apk,sharing=locked \
    --mount=type=secret,id=tok,target=/run/secrets/tok,required=true,env=TOKEN \
    --mount=type=bind,source=scripts,target=/mnt/s,ro \
    --mount=type=tmpfs,target=/tmp/scratch \
    <<-EOF
	set -eu
	# comments inside heredocs stay intact
	echo "hi"
	EOF
RUN <<PY
#!/usr/bin/env python3
print(1 + 2)
PY
RUN cat <<MSG > /etc/msg
hello world
MSG
COPY --chmod=755 <<EOF /usr/local/bin/greet
echo "hello $WHO"
EOF
COPY <<'RAW' <<a.txt /etc/conf/
$LITERAL
RAW
second file
a.txt
ADD --chmod=0644 --checksum=sha256:abcdef https://example.com/app.tar.gz /opt/
"#;
        let df = Dockerfile::parse(raw).unwrap();
        let ins = &df.stages[0].instructions;
        assert_eq!(ins.len(), 6);

        let Kind::Run { command, mounts, stdin } = &ins[0].kind else { panic!("not run") };
        assert_eq!(*command, Command::Shell("set -eu\n# comments inside heredocs stay intact\necho \"hi\"\n".into()));
        assert_eq!(*stdin, None);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0], RunMount::Cache { target: "/var/cache/apk".into(), id: Some("apk".into()), read_only: false, mode: None });
        assert_eq!(
            mounts[1],
            RunMount::Secret { id: "tok".into(), target: Some("/run/secrets/tok".into()), env: Some("TOKEN".into()), required: true, mode: None }
        );
        assert_eq!(mounts[2], RunMount::Bind { source: "scripts".into(), target: "/mnt/s".into(), from: None, read_write: false });
        assert_eq!(mounts[3], RunMount::Tmpfs { target: "/tmp/scratch".into() });

        let Kind::Run { command, stdin, .. } = &ins[1].kind else { panic!("not shebang run") };
        assert_eq!(*command, Command::Exec(vec!["/usr/bin/env".into(), "python3".into(), "/proc/self/fd/0".into()]));
        assert_eq!(stdin.as_deref(), Some("#!/usr/bin/env python3\nprint(1 + 2)\n"));

        let Kind::Run { command, stdin, .. } = &ins[2].kind else { panic!("not inline heredoc") };
        assert_eq!(*command, Command::Shell("cat <<MSG > /etc/msg\nhello world\nMSG\n".into()));
        assert_eq!(*stdin, None);

        let Kind::Copy { sources, heredocs, dest, chmod, .. } = &ins[3].kind else { panic!("not copy heredoc") };
        assert!(sources.is_empty());
        assert_eq!(dest, "/usr/local/bin/greet");
        assert_eq!(chmod.as_deref(), Some("755"));
        assert_eq!(heredocs, &[Heredoc { name: "EOF".into(), content: "echo \"hello $WHO\"\n".into(), expand: true }]);

        let Kind::Copy { heredocs, dest, .. } = &ins[4].kind else { panic!("not multi copy heredoc") };
        assert_eq!(dest, "/etc/conf/");
        assert_eq!(
            heredocs,
            &[
                Heredoc { name: "RAW".into(), content: "$LITERAL\n".into(), expand: false },
                Heredoc { name: "a.txt".into(), content: "second file\n".into(), expand: true },
            ]
        );

        let Kind::Copy { sources, dest, chmod, checksum, add, .. } = &ins[5].kind else { panic!("not add url") };
        assert!(*add);
        assert_eq!(sources, &["https://example.com/app.tar.gz"]);
        assert_eq!(dest, "/opt/");
        assert_eq!(chmod.as_deref(), Some("0644"));
        assert_eq!(checksum.as_deref(), Some("sha256:abcdef"));
    }

    #[test]
    fn refuses_what_it_cannot_do() {
        let e = Dockerfile::parse("FROM alpine\nRUN <<EOF\necho hi\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("unterminated heredoc"));
        assert!(Dockerfile::parse("RUN echo\n").unwrap_err().message.contains("before the first FROM"));
        assert!(Dockerfile::parse("FROM a\nCOPY https://x/y.tgz /\n").unwrap_err().message.contains("URL"));
        assert!(Dockerfile::parse("FROM a\nADD ftp://x/y.tgz /\n").unwrap_err().message.contains("http"));
        assert!(Dockerfile::parse("FROM a\nCOPY --chmod=999 a b\n").unwrap_err().message.contains("octal"));
        assert!(Dockerfile::parse("FROM a\nRUN --mount=type=ssh cat\n").is_err());
        assert!(Dockerfile::parse("FROM a\nFROBNICATE\n").unwrap_err().message.contains("unknown"));
        assert!(Dockerfile::parse("# only a comment\n").is_err());
        // Bit-shifts and here-strings are not heredocs.
        assert!(Dockerfile::parse("FROM a\nRUN echo $((1 << 2)) && echo $((1<<2)) && cat <<<hi\n").is_ok());
    }

    #[test]
    fn expands_variables() {
        let vars: BTreeMap<String, String> = [("A".to_string(), "x".to_string()), ("EMPTY".to_string(), String::new())].into();
        assert_eq!(expand("$A/${A}y", &vars), "x/xy");
        assert_eq!(expand("${MISSING:-def}-${EMPTY:-e}-${A:+set}-${MISSING:+no}", &vars), "def-e-set-");
        assert_eq!(expand(r"\$A costs $5", &vars), "$A costs $5");
        assert_eq!(split_words(r#"a "b c" d\ e 'f'"#), ["a", "b c", "d e", "f"]);
    }
}
