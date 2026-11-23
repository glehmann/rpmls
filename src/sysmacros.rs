//! The definitions of the macros rpm itself provides, in its macro files.
//!
//! rpm is asked for its macro path, so the macros are those of the system
//! rpmls runs in: the host, or a container of the target distribution. A
//! container's files are not visible to the editor on the host, so with a
//! cache directory, shared with the host at the same path, the files are
//! copied there and the locations point at the copies.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{Location, Position, Range, Uri};

pub struct SystemMacros {
    /// Macro name → the files and 0-based lines defining it, in rpm's order.
    index: HashMap<String, Vec<(PathBuf, u32)>>,
    cache: Option<PathBuf>,
}

impl SystemMacros {
    pub fn load(cache: Option<PathBuf>) -> Result<Self> {
        let mut index: HashMap<String, Vec<(PathBuf, u32)>> = HashMap::new();
        for path in macro_files()? {
            let Ok(text) = fs::read_to_string(&path) else { continue };
            for (name, line) in definitions(&text) {
                index.entry(name).or_default().push((path.clone(), line));
            }
        }
        Ok(Self { index, cache })
    }

    pub fn names(&self) -> Vec<String> {
        self.index.keys().cloned().collect()
    }

    /// The definitions of `name`, as written in rpm's files, with the files.
    pub fn definition_texts(&self, name: &str) -> Vec<(PathBuf, String)> {
        let Some(defs) = self.index.get(name) else { return Vec::new() };
        defs.iter()
            .filter_map(|(path, line)| {
                let text = fs::read_to_string(path).ok()?;
                Some((path.clone(), block(&text, *line as usize)))
            })
            .collect()
    }

    pub fn definitions(&self, name: &str) -> Result<Vec<Location>> {
        let Some(defs) = self.index.get(name) else { return Ok(Vec::new()) };
        defs.iter()
            .map(|(path, line)| {
                let path = match &self.cache {
                    Some(cache) => cached_copy(cache, path)?,
                    None => path.clone(),
                };
                let uri = url::Url::from_file_path(&path)
                    .map_err(|()| anyhow!("not an absolute path: {}", path.display()))?;
                let uri: Uri = uri.as_str().parse()?;
                let start = Position::new(*line, 0);
                Ok(Location::new(uri, Range::new(start, start)))
            })
            .collect()
    }
}

/// rpm's macro files, from the macro path `rpm --showrc` reports, with its
/// macros (like `%{_target}`) expanded and its globs resolved.
fn macro_files() -> Result<Vec<PathBuf>> {
    let showrc = rpm(&["--showrc"])?;
    let path = showrc
        .lines()
        .find_map(|l| l.strip_prefix("Macro path: "))
        .ok_or_else(|| anyhow!("rpm --showrc reports no macro path"))?;
    let path = rpm(&["--eval", path])?;
    let mut files = Vec::new();
    for pattern in path.trim().split(':').filter(|p| !p.is_empty()) {
        let mut matches: Vec<PathBuf> = glob::glob(pattern)?.filter_map(Result::ok).collect();
        matches.sort();
        files.extend(matches.into_iter().filter(|p| p.is_file()));
    }
    Ok(files)
}

fn rpm(args: &[&str]) -> Result<String> {
    let output = Command::new("rpm").args(args).output().context("running rpm")?;
    if !output.status.success() {
        bail!("rpm {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The macros a macro file defines, with their 0-based lines: a line starting
/// with `%name`, followed by blanks or parametric options, that does not
/// continue the previous line.
fn definitions(text: &str) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    let mut continued = false;
    for (i, line) in text.lines().enumerate() {
        let continues = continued;
        continued = line.ends_with('\\');
        if continues {
            continue;
        }
        let Some(rest) = line.trim_start().strip_prefix('%') else { continue };
        let end =
            rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        if !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && after.starts_with([' ', '\t', '('])
        {
            found.push((name.to_string(), i as u32));
        }
    }
    found
}

/// The macro definition starting at 0-based `line`, with its continuation
/// lines, up to 20 lines.
pub fn block(text: &str, line: usize) -> String {
    let mut lines = Vec::new();
    for l in text.lines().skip(line).take(20) {
        lines.push(l);
        if !l.ends_with('\\') {
            break;
        }
    }
    lines.join("\n")
}

/// A read-only copy of `path` under `cache`, at its full path, refreshed when
/// the original changed.
fn cached_copy(cache: &Path, path: &Path) -> Result<PathBuf> {
    let copy = cache.join(path.strip_prefix("/").unwrap_or(path));
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    if modified(&copy).is_none() || modified(&copy) < modified(path) {
        if let Some(dir) = copy.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        if copy.exists() {
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o644))
                .with_context(|| format!("replacing {}", copy.display()))?;
        }
        fs::copy(path, &copy)
            .with_context(|| format!("copying {} to {}", path.display(), copy.display()))?;
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o444))?;
    }
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_skip_continuations_and_comments() {
        let text = "\
# %not_a_definition here
%_bindir\t\t%{_exec_prefix}/bin
%cmake \\
  %cmake_build \\
  more
%py_setup(n:) setup.py
  %indented value
%{expand:%%dont}
%1 positional
";
        let names: Vec<_> = definitions(text).into_iter().collect();
        assert_eq!(
            names,
            [
                ("_bindir".to_string(), 1),
                ("cmake".to_string(), 2),
                ("py_setup".to_string(), 5),
                ("indented".to_string(), 6),
            ]
        );
    }

    #[test]
    fn block_follows_continuations() {
        let text = "%a 1\n%cmake \\\n  -DX=1 \\\n  -DY=2\n%b 2\n";
        assert_eq!(block(text, 0), "%a 1");
        assert_eq!(block(text, 1), "%cmake \\\n  -DX=1 \\\n  -DY=2");
    }

    #[test]
    fn cached_copies_are_read_only_and_refreshed() {
        let dir = std::env::temp_dir().join(format!("rpmls-test-{}", std::process::id()));
        let source = dir.join("src/macros.test");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "%a 1\n").unwrap();
        let cache = dir.join("cache");
        let copy = cached_copy(&cache, &source).unwrap();
        assert!(copy.starts_with(&cache) && copy.ends_with("src/macros.test"));
        assert_eq!(fs::read_to_string(&copy).unwrap(), "%a 1\n");
        assert!(fs::metadata(&copy).unwrap().permissions().readonly());
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&source, "%a 2\n").unwrap();
        assert_eq!(fs::read_to_string(cached_copy(&cache, &source).unwrap()).unwrap(), "%a 2\n");
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
