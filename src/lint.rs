//! Linting with rpmcrab, or rpmlint, on a background thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Uri};
use serde::Deserialize;

/// How long the documents must stay unchanged before they are linted.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// The linter, and its configuration beyond what it finds on its own.
#[derive(Default)]
pub struct Config {
    pub linter: Linter,
    /// Configuration files or directories (`-c`).
    pub files: Vec<PathBuf>,
    /// A directory of the filters of each package, `<package>.rpmlintrc`,
    /// the package named after the directory of its spec, loaded after the
    /// rpmlintrc files beside the spec.
    pub rpmlintrc_dir: Option<PathBuf>,
}

impl Config {
    /// The rpmlintrc of the package of `spec` in the rpmlintrc directory.
    pub fn package_rpmlintrc(&self, spec: &Path) -> Option<PathBuf> {
        let package = spec.parent()?.file_name()?.to_string_lossy();
        Some(self.rpmlintrc_dir.as_ref()?.join(format!("{package}.rpmlintrc")))
    }

    /// The linter's arguments for `spec`: with a package rpmlintrc, or
    /// rpmlint, which lints a copy of the spec, all the rpmlintrc files are
    /// given, as `-r` stops the linter looking beside the spec.
    fn args(&self, spec: &Path) -> Vec<PathBuf> {
        let mut args = vec![PathBuf::from("--ignore-unused-rpmlintrc")];
        for file in &self.files {
            args.extend([PathBuf::from("-c"), file.clone()]);
        }
        let own = self.package_rpmlintrc(spec);
        if own.is_some() || self.linter == Linter::Rpmlint {
            let beside = spec.parent().map(rpmlintrc_files).unwrap_or_default();
            for rc in beside.into_iter().chain(own.filter(|p| p.is_file())) {
                args.extend([PathBuf::from("-r"), rc]);
            }
        }
        args
    }

    /// The linter's explanation of a check (`--explain`), without the
    /// leading `check:` line.
    pub fn explain(&self, check: &str) -> Result<String> {
        let name = self.linter.name();
        let output = Command::new(name)
            .args(["--explain", check])
            .output()
            .with_context(|| format!("running {name}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.split_once('\n').map_or("", |(_, rest)| rest).trim();
        if text.is_empty() || text.starts_with("Unknown message") {
            return Err(anyhow!("{name} has no explanation for {check}"));
        }
        Ok(text.to_string())
    }
}

#[derive(Default, PartialEq, Clone, Copy)]
pub enum Linter {
    /// Fast, and safe on a buffer being typed: the spec is not parsed by rpm.
    #[default]
    Rpmcrab,
    /// rpm parses the spec, which runs its shell and Lua code: in a
    /// container only, without network nor write access to the project.
    Rpmlint,
}

impl Linter {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rpmcrab => "rpmcrab",
            Self::Rpmlint => "rpmlint",
        }
    }
}

/// The rpmlintrc files rpmcrab loads from `dir`: `*.rpmlintrc`, then
/// `*-rpmlintrc`, each sorted.
pub fn rpmlintrc_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    files.retain(|p| p.is_file());
    files.sort();
    [".rpmlintrc", "-rpmlintrc"]
        .iter()
        .flat_map(|suffix| files.iter().filter(move |p| p.to_string_lossy().ends_with(suffix)))
        .cloned()
        .collect()
}

/// Work for the lint thread.
pub enum Job {
    /// The text of an open document, linted once it stops changing.
    Lint { uri: Uri, version: i32, text: String },
    /// A document closed: its spec is linted as it is on disk again.
    Close(Uri),
    /// Specs to lint as they are on disk, but those open, of which the text
    /// is: when nothing else is to lint, as the open documents come first.
    Files(Vec<PathBuf>),
}

/// Lint the latest text of each changed document once no change has come in
/// for [`DEBOUNCE`], and the specs on disk in between, until the job channel
/// closes, handing each document's diagnostics, and its version, to
/// `publish`.
pub fn lint_loop(
    jobs: mpsc::Receiver<Job>,
    config: &Config,
    publish: impl Fn(Uri, Option<i32>, Vec<Diagnostic>),
) {
    // Keyed by the URI's text: `Uri` has interior mutability.
    let mut open: HashMap<String, (Uri, i32, String)> = HashMap::new();
    let mut pending: HashSet<String> = HashSet::new();
    let mut files: VecDeque<PathBuf> = VecDeque::new();
    let report = |uri: Uri, version, result: Result<Vec<Diagnostic>>| match result {
        Ok(diagnostics) => publish(uri, version, diagnostics),
        Err(e) => eprintln!("rpmls: linting {}: {e:#}", uri.as_str()),
    };
    loop {
        let job = if !pending.is_empty() {
            match jobs.recv_timeout(DEBOUNCE) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    for (uri, version, text) in pending.drain().filter_map(|key| open.get(&key)) {
                        report(uri.clone(), Some(*version), lint(uri, text, config));
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else if let Some(path) = files.front().cloned() {
            match jobs.try_recv() {
                Ok(job) => job,
                Err(TryRecvError::Empty) => {
                    files.pop_front();
                    let Some(uri) = crate::links::file_uri(&path) else { continue };
                    if let Some((uri, version, text)) = open.get(uri.as_str()) {
                        report(uri.clone(), Some(*version), lint(uri, text, config));
                    } else {
                        match std::fs::read_to_string(&path) {
                            Ok(text) => report(uri.clone(), None, lint(&uri, &text, config)),
                            // Gone, with its findings.
                            Err(_) => publish(uri, None, Vec::new()),
                        }
                    }
                    continue;
                }
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        match job {
            Job::Lint { uri, version, text } => {
                let key = uri.as_str().to_string();
                pending.insert(key.clone());
                open.insert(key, (uri, version, text));
            }
            Job::Close(uri) => {
                pending.remove(uri.as_str());
                open.remove(uri.as_str());
                let path = url::Url::parse(uri.as_str()).ok().and_then(|u| u.to_file_path().ok());
                files.extend(path.filter(|p| !files.contains(p)));
            }
            Job::Files(paths) => {
                for path in paths {
                    if !files.contains(&path) {
                        files.push_back(path);
                    }
                }
            }
        }
    }
}

/// rpmcrab's `--format json` report, reduced to what the diagnostics use.
#[derive(Deserialize)]
struct Report {
    findings: Vec<Finding>,
}

#[derive(Deserialize)]
struct Finding {
    level: String,
    check: String,
    line: Option<u32>,
    details: Vec<String>,
}

/// Lint `text` as the spec at `uri`. The filters unused, like those of the
/// binary packages, are not reported.
fn lint(uri: &Uri, text: &str, config: &Config) -> Result<Vec<Diagnostic>> {
    let path =
        url::Url::parse(uri.as_str())?.to_file_path().map_err(|()| anyhow!("not a file URI"))?;
    let findings = match config.linter {
        Linter::Rpmcrab => rpmcrab(&path, text, config)?,
        Linter::Rpmlint => rpmlint(&path, text, config)?,
    };
    let lines: Vec<&str> = text.lines().collect();
    let source = config.linter.name();
    Ok(findings.into_iter().map(|finding| diagnostic(finding, &lines, source)).collect())
}

/// rpmcrab's findings, from the spec on its stdin, not parsed by rpm
/// (`--no-spec-parse`), which would run the shell and Lua code of a
/// half-typed buffer.
fn rpmcrab(path: &Path, text: &str, config: &Config) -> Result<Vec<Finding>> {
    let mut child = Command::new("rpmcrab")
        .args(["--format", "json", "--no-spec-parse"])
        .args(config.args(path))
        .arg("--stdin-filename")
        .arg(path)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running rpmcrab")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).context("writing the spec to rpmcrab")?;
    }
    let output = child.wait_with_output().context("waiting for rpmcrab")?;
    let report: Report = serde_json::from_slice(&output.stdout).with_context(|| {
        format!("reading rpmcrab's report: {}", String::from_utf8_lossy(&output.stderr).trim())
    })?;
    Ok(report.findings)
}

/// rpmlint's findings, from a copy of the spec, as it reads no stdin, in a
/// directory of its own linking to the other files of the spec's, its
/// sources, for `%include %{SOURCE1}`: the directory is replaced by the
/// spec's in the details.
fn rpmlint(path: &Path, text: &str, config: &Config) -> Result<Vec<Finding>> {
    let name = path.file_name().ok_or_else(|| anyhow!("no file name"))?.to_string_lossy();
    let spec_dir = path.parent().ok_or_else(|| anyhow!("no directory"))?;
    let dir = std::env::temp_dir().join(format!("rpmls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for entry in std::fs::read_dir(spec_dir).into_iter().flatten().flatten() {
        if entry.file_name() != path.file_name().unwrap_or_default() {
            let _ = std::os::unix::fs::symlink(entry.path(), dir.join(entry.file_name()));
        }
    }
    let copy = dir.join(&*name);
    std::fs::write(&copy, text).with_context(|| format!("writing {}", copy.display()))?;
    let output = Command::new("rpmlint").args(config.args(path)).arg(&copy).output();
    let _ = std::fs::remove_dir_all(&dir);
    let output = output.context("running rpmlint")?;
    // 64 for errors, 66 for a badness over the threshold.
    if !matches!(output.status.code(), Some(0 | 64 | 66)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("rpmlint failed: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout)
        .replace(&*dir.to_string_lossy(), &spec_dir.to_string_lossy());
    Ok(stdout.lines().filter_map(|line| finding(line.strip_prefix(&*name)?)).collect())
}

/// A finding of rpmlint's output, after the spec's name:
/// `[:<line>]: <level>: <check>[ <details>]`.
fn finding(rest: &str) -> Option<Finding> {
    let rest = rest.strip_prefix(':')?;
    let (line, rest) = match rest.split_once(": ") {
        Some((line, rest)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => {
            (line.parse().ok(), rest)
        }
        _ => (None, rest.strip_prefix(' ')?),
    };
    let (level, rest) = rest.split_once(": ")?;
    if !matches!(level, "E" | "W" | "I") {
        return None;
    }
    let (check, details) = rest.split_once(' ').unwrap_or((rest, ""));
    let details = (!details.is_empty()).then(|| details.to_string()).into_iter().collect();
    Some(Finding { level: level.to_string(), check: check.to_string(), line, details })
}

/// A diagnostic covering the finding's whole line; a finding about the whole
/// spec has no line and goes on the first one.
fn diagnostic(finding: Finding, lines: &[&str], source: &str) -> Diagnostic {
    let line = finding.line.map_or(0, |n| n.saturating_sub(1));
    let end = lines.get(line as usize).map_or(0, |l| l.encode_utf16().count() as u32);
    let severity = match finding.level.as_str() {
        "E" => DiagnosticSeverity::ERROR,
        "W" => DiagnosticSeverity::WARNING,
        _ => DiagnosticSeverity::INFORMATION,
    };
    let message = std::iter::once(finding.check.as_str())
        .chain(finding.details.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    Diagnostic {
        range: Range::new(Position::new(line, 0), Position::new(line, end)),
        severity: Some(severity),
        code: Some(NumberOrString::String(finding.check)),
        source: Some(source.to_string()),
        message,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_rpmlintrc_follows_those_beside_the_spec() {
        let dir = std::env::temp_dir().join(format!("rpmls-lint-{}", std::process::id()));
        let (pkg, lint) = (dir.join("base/foo"), dir.join("lint"));
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&lint).unwrap();
        for file in [pkg.join("b-rpmlintrc"), pkg.join("a.rpmlintrc"), lint.join("foo.rpmlintrc")] {
            std::fs::write(file, "").unwrap();
        }
        let spec = pkg.join("foo.spec");
        let mut config = Config {
            files: vec!["/c".into()],
            rpmlintrc_dir: Some(lint.clone()),
            ..Default::default()
        };
        let r = |p: &Path| ["-r".into(), p.to_path_buf()];
        let beside = [r(&pkg.join("a.rpmlintrc")), r(&pkg.join("b-rpmlintrc"))];
        let expected: Vec<PathBuf> = ["--ignore-unused-rpmlintrc".into(), "-c".into(), "/c".into()]
            .into_iter()
            .chain(beside.iter().flatten().cloned())
            .chain(r(&lint.join("foo.rpmlintrc")))
            .collect();
        assert_eq!(config.args(&spec), expected);
        // Without one, those beside the spec only.
        std::fs::remove_file(lint.join("foo.rpmlintrc")).unwrap();
        assert_eq!(config.args(&spec)[3..], beside.concat());
        // rpmcrab finds them on its own, rpmlint lints a copy elsewhere.
        assert_eq!(Config::default().args(&spec).len(), 1);
        config = Config { linter: Linter::Rpmlint, ..Default::default() };
        assert_eq!(config.args(&spec)[1..], beside.concat());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rpmlint_findings_are_read_from_its_output() {
        let f = |rest: &str| finding(rest).map(|f| (f.level, f.check, f.line, f.details));
        let s = |text: &str| text.to_string();
        assert_eq!(
            f(":14: W: unversioned-explicit-provides foo"),
            Some((s("W"), s("unversioned-explicit-provides"), Some(14), vec![s("foo")]))
        );
        assert_eq!(
            f(": W: no-%check-section"),
            Some((s("W"), s("no-%check-section"), None, vec![]))
        );
        assert_eq!(f(" 0 packages and 1 specfiles checked; 0 errors"), None);
        assert_eq!(f(".rpmlintrc: E: x"), None);
    }
}
