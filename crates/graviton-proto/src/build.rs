//! Path matching shared by the host and the guest for image builds: COPY
//! sources with wildcards, and `.dockerignore`. gravitond hashes what a COPY
//! will take (for the build cache) with the same rules the agent copies by.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Does one path component match a glob: `*`, `?` and `[a-z]` / `[!a]` classes.
pub fn match_component(pattern: &str, name: &str) -> bool {
    fn inner(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| inner(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && inner(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().skip(1).position(|&c| c == ']').map(|i| i + 1) else {
                    return n.first() == Some(&'[') && inner(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else { return false };
                let mut class = &p[1..close];
                let negate = matches!(class.first(), Some('!') | Some('^'));
                if negate {
                    class = &class[1..];
                }
                let mut hit = false;
                let mut i = 0;
                while i < class.len() {
                    if i + 2 < class.len() && class[i + 1] == '-' {
                        hit |= class[i] <= c && c <= class[i + 2];
                        i += 3;
                    } else {
                        hit |= class[i] == c;
                        i += 1;
                    }
                }
                hit != negate && inner(&p[close + 1..], &n[1..])
            }
            Some('\\') if p.len() > 1 => n.first() == Some(&p[1]) && inner(&p[2..], &n[1..]),
            Some(&c) => n.first() == Some(&c) && inner(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    inner(&p, &n)
}

fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// The normal components of a path: no `.`, no leading `/`; `..` pops.
pub fn components(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// `.dockerignore`: the last matching rule wins, `!` re-includes, and a rule
/// that matches a folder matches everything in it. `**` matches any number of
/// folders.
#[derive(Debug, Clone, Default)]
pub struct Ignore {
    rules: Vec<(bool, Vec<String>)>,
}

impl Ignore {
    pub fn new(patterns: &[String]) -> Self {
        let rules = patterns
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| match l.strip_prefix('!') {
                Some(rest) => (true, components(rest.trim())),
                None => (false, components(l)),
            })
            .filter(|(_, c)| !c.is_empty())
            .collect();
        Self { rules }
    }

    /// The lines of a `.dockerignore` file.
    pub fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// `rel` is relative to the context root.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        let parts: Vec<String> = rel.components().filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        }).collect();
        let mut ignored = false;
        for (negate, rule) in &self.rules {
            // The path itself or any folder above it.
            let hit = (1..=parts.len()).any(|n| matches_parts(rule, &parts[..n]));
            if hit {
                ignored = !negate;
            }
        }
        ignored
    }
}

fn matches_parts(rule: &[String], parts: &[String]) -> bool {
    match rule.first() {
        None => parts.is_empty(),
        Some(r) if r == "**" => (0..=parts.len()).any(|i| matches_parts(&rule[1..], &parts[i..])),
        Some(r) => !parts.is_empty() && match_component(r, &parts[0]) && matches_parts(&rule[1..], &parts[1..]),
    }
}

/// Expand a COPY source (relative to `base`, wildcards allowed in any
/// component) to the paths it names, relative to `base`, sorted. Ignored
/// paths are left out. Errors if nothing matches.
pub fn expand_source(base: &Path, source: &str, ignore: &Ignore) -> io::Result<Vec<PathBuf>> {
    let parts = components(source);
    let mut found = vec![PathBuf::new()];
    for part in &parts {
        let mut next = Vec::new();
        for dir in &found {
            if has_glob(part) {
                let Ok(entries) = fs::read_dir(base.join(dir)) else { continue };
                let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
                names.sort();
                next.extend(names.into_iter().filter(|n| match_component(part, n)).map(|n| dir.join(n)));
            } else {
                let p = dir.join(part);
                if fs::symlink_metadata(base.join(&p)).is_ok() {
                    next.push(p);
                }
            }
        }
        found = next;
    }
    found.retain(|p| p.as_os_str().is_empty() || !ignore.is_ignored(p));
    if found.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("COPY source `{source}` matched no files")));
    }
    found.sort();
    Ok(found)
}

/// `rel` and everything under it (folders aren't followed through symlinks),
/// relative to `base`, leaving out ignored paths. Sorted.
pub fn walk(base: &Path, rel: &Path, ignore: &Ignore) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![rel.to_path_buf()];
    while let Some(p) = stack.pop() {
        if !p.as_os_str().is_empty() && ignore.is_ignored(&p) {
            continue;
        }
        let meta = fs::symlink_metadata(base.join(&p))?;
        if meta.is_dir() {
            for e in fs::read_dir(base.join(&p))? {
                stack.push(p.join(e?.file_name()));
            }
        }
        out.push(p);
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(match_component("*.py", "app.py"));
        assert!(!match_component("*.py", "app.pyc"));
        assert!(match_component("file?.txt", "file1.txt"));
        assert!(match_component("[a-c]x", "bx"));
        assert!(!match_component("[!a-c]x", "bx"));
        assert!(match_component("*", ".env"));
    }

    #[test]
    fn dockerignore() {
        let ig = Ignore::new(&Ignore::lines("# deps\nnode_modules\n*.log\n**/__pycache__\n.git\n!keep.log\n"));
        assert!(ig.is_ignored(Path::new("node_modules")));
        assert!(ig.is_ignored(Path::new("node_modules/x/y.js")));
        assert!(ig.is_ignored(Path::new("debug.log")));
        assert!(!ig.is_ignored(Path::new("keep.log")));
        assert!(!ig.is_ignored(Path::new("src/debug.log"))); // *.log is root-only, like Docker
        assert!(ig.is_ignored(Path::new("src/pkg/__pycache__/a.pyc")));
        assert!(!ig.is_ignored(Path::new("src/main.py")));
    }

    #[test]
    fn expands_sources() {
        let dir = std::env::temp_dir().join(format!("graviton-build-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/pkg")).unwrap();
        for f in ["a.py", "b.py", "README.md", "src/main.py", "src/pkg/x.py", "secret.env"] {
            fs::write(dir.join(f), f).unwrap();
        }
        let ig = Ignore::new(&["*.env".into()]);
        let got = expand_source(&dir, "*.py", &ig).unwrap();
        assert_eq!(got, [PathBuf::from("a.py"), PathBuf::from("b.py")]);
        assert_eq!(expand_source(&dir, "./src/../src", &ig).unwrap(), [PathBuf::from("src")]);
        assert!(expand_source(&dir, "secret.env", &ig).is_err());
        assert!(expand_source(&dir, "nope/*", &ig).is_err());
        let all = walk(&dir, Path::new(""), &ig).unwrap();
        assert!(all.contains(&PathBuf::from("src/pkg/x.py")));
        assert!(!all.contains(&PathBuf::from("secret.env")));
        assert_eq!(expand_source(&dir, ".", &ig).unwrap(), [PathBuf::new()]);
        let _ = fs::remove_dir_all(dir);
    }
}
