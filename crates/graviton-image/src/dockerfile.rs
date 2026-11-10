//! A Dockerfile parser for `grav build`.
//!
//! Supported: FROM (with `AS name` and earlier stages as bases), RUN, CMD,
//! ENTRYPOINT and SHELL in shell or JSON form, COPY and ADD (local files,
//! `--from`, `--chown`), ENV, ARG, WORKDIR, USER, EXPOSE, LABEL, VOLUME,
//! STOPSIGNAL, and line continuations. `RUN --mount=type=cache` runs without
//! the cache. Heredocs, ONBUILD, `ADD <url>` and `--chmod` are refused with a
//! clear message.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `["prog", "arg"]`
    Exec(Vec<String>),
    /// Run through the stage's SHELL (`/bin/sh -c` by default).
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Run(Command),
    Cmd(Command),
    Entrypoint(Command),
    Shell(Vec<String>),
    Copy { sources: Vec<String>, dest: String, from: Option<String>, chown: Option<String>, add: bool },
    Env(Vec<(String, String)>),
    Arg(String, Option<String>),
    Workdir(String),
    User(String),
    Expose(Vec<String>),
    Label(Vec<(String, String)>),
    Volume(Vec<String>),
    StopSignal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// 1-based line where it starts.
    pub line: usize,
    /// As written (continuations joined), for the build log and the cache key.
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

impl Dockerfile {
    pub fn parse(text: &str) -> Result<Self, ParseError> {
        let mut df = Dockerfile::default();
        for (line, raw) in logical_lines(text) {
            let err = |message: String| ParseError { line, message };
            let trimmed = raw.trim();
            let (word, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
            let rest = rest.trim();
            let instr = word.to_ascii_uppercase();
            if rest.contains("<<") && matches!(instr.as_str(), "RUN" | "COPY" | "ADD") && heredoc(rest) {
                return Err(err("heredocs aren't supported yet; use a script file or RUN with && instead".into()));
            }
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
                    while let Some(flag) = rest.strip_prefix("--") {
                        let (f, after) = flag.split_once(char::is_whitespace).unwrap_or((flag, ""));
                        if f.starts_with("mount=") && !f.contains("type=cache") {
                            return Err(err(format!("RUN --{f} isn't supported (only type=cache mounts, which run without the cache)")));
                        }
                        if !f.starts_with("mount=") && !f.starts_with("network=") {
                            return Err(err(format!("RUN --{f} isn't supported")));
                        }
                        rest = after.trim_start();
                    }
                    Kind::Run(command(rest))
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
                    let mut rest = rest;
                    while rest.starts_with("--") {
                        let (flag, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                        if let Some(v) = flag.strip_prefix("--from=") {
                            from = Some(unquote(v));
                        } else if let Some(v) = flag.strip_prefix("--chown=") {
                            chown = Some(unquote(v));
                        } else if flag != "--link" && !flag.starts_with("--link=") {
                            return Err(err(format!("{instr} {flag} isn't supported")));
                        }
                        rest = after.trim_start();
                    }
                    let mut paths = json_array(rest).unwrap_or_else(|| split_words(rest));
                    if paths.len() < 2 {
                        return Err(err(format!("{instr} needs at least one source and a destination")));
                    }
                    let dest = paths.pop().unwrap();
                    if add && paths.iter().any(|p| p.contains("://")) {
                        return Err(err("ADD from a URL isn't supported; use RUN with curl or wget".into()));
                    }
                    Kind::Copy { sources: paths, dest, from, chown, add }
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
                "HEALTHCHECK" => continue, // not run by Graviton yet
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

/// Join continuation lines (`\` at the end) and drop comments and blanks.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (i, line) in text.lines().enumerate() {
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
            None => current = Some((i + 1, content.trim().to_string())),
        }
        if !continued {
            if let Some((n, s)) = current.take().filter(|(_, s)| !s.trim().is_empty()) {
                out.push((n, s));
            }
        }
    }
    if let Some((n, s)) = current.filter(|(_, s)| !s.trim().is_empty()) {
        out.push((n, s));
    }
    out
}

fn heredoc(rest: &str) -> bool {
    rest.split_whitespace().any(|w| w.strip_prefix("<<").is_some_and(|tag| {
        let tag = tag.trim_start_matches('-').trim_matches(['"', '\'']);
        !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }))
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
COPY --chown=app:app requirements.txt ./
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
        assert_eq!(build.instructions[2].kind, Kind::Run(Command::Shell("pip install --upgrade pip && echo done".into())));
        assert_eq!(build.instructions[2].line, 9);
        assert_eq!(
            build.instructions[3].kind,
            Kind::Copy { sources: vec!["requirements.txt".into()], dest: "./".into(), from: None, chown: Some("app:app".into()), add: false }
        );
        assert_eq!(build.instructions[4].kind, Kind::Copy { sources: vec!["src dir/".into()], dest: "/src/".into(), from: None, chown: None, add: false });
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
    fn refuses_what_it_cannot_do() {
        let e = Dockerfile::parse("FROM alpine\nRUN <<EOF\necho hi\nEOF\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.contains("heredocs"));
        assert!(Dockerfile::parse("RUN echo\n").unwrap_err().message.contains("before the first FROM"));
        assert!(Dockerfile::parse("FROM a\nADD https://x/y.tgz /\n").unwrap_err().message.contains("URL"));
        assert!(Dockerfile::parse("FROM a\nRUN --mount=type=secret,id=x cat\n").is_err());
        assert!(Dockerfile::parse("FROM a\nFROBNICATE\n").unwrap_err().message.contains("unknown"));
        assert!(Dockerfile::parse("# only a comment\n").is_err());
        // A bit-shift isn't a heredoc.
        assert!(Dockerfile::parse("FROM a\nRUN echo $((1 << 2))\n").is_ok());
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
