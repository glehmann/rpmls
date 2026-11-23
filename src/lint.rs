//! Linting with rpmcrab on a background thread.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use lsp_server::{Message, Notification};
use lsp_types::notification::{Notification as _, PublishDiagnostics};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, PublishDiagnosticsParams, Range, Uri,
};
use serde::Deserialize;

/// How long the documents must stay unchanged before they are linted.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Work for the lint thread.
pub enum Job {
    Lint { uri: Uri, version: i32, text: String },
    Close(Uri),
}

/// Lint the latest text of each changed document once no change has come in
/// for [`DEBOUNCE`], until the job channel closes.
pub fn lint_loop(jobs: mpsc::Receiver<Job>, send: impl Fn(Message)) {
    // Keyed by the URI's text: `Uri` has interior mutability.
    let mut pending: HashMap<String, (Uri, i32, String)> = HashMap::new();
    loop {
        let job = if pending.is_empty() {
            match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        } else {
            match jobs.recv_timeout(DEBOUNCE) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => {
                    for (uri, version, text) in pending.drain().map(|(_, v)| v) {
                        match lint(&uri, &text) {
                            Ok(diagnostics) => publish(&send, uri, Some(version), diagnostics),
                            Err(e) => eprintln!("rpmls: linting {}: {e:#}", uri.as_str()),
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        match job {
            Job::Lint { uri, version, text } => {
                pending.insert(uri.as_str().to_string(), (uri, version, text));
            }
            Job::Close(uri) => {
                pending.remove(uri.as_str());
                publish(&send, uri, None, Vec::new());
            }
        }
    }
}

fn publish(send: &impl Fn(Message), uri: Uri, version: Option<i32>, diagnostics: Vec<Diagnostic>) {
    let params = PublishDiagnosticsParams { uri, diagnostics, version };
    send(Notification::new(PublishDiagnostics::METHOD.to_string(), params).into());
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

/// Lint `text` with rpmcrab as the spec at `uri`. The spec is not parsed by
/// rpm (`--no-spec-parse`), which would run the shell and Lua code of a
/// half-typed buffer.
fn lint(uri: &Uri, text: &str) -> Result<Vec<Diagnostic>> {
    let path =
        url::Url::parse(uri.as_str())?.to_file_path().map_err(|()| anyhow!("not a file URI"))?;
    let mut child = Command::new("rpmcrab")
        .args(["--format", "json", "--no-spec-parse", "--stdin-filename"])
        .arg(&path)
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
    let lines: Vec<&str> = text.lines().collect();
    Ok(report.findings.into_iter().map(|finding| diagnostic(finding, &lines)).collect())
}

/// A diagnostic covering the finding's whole line; a finding about the whole
/// spec has no line and goes on the first one.
fn diagnostic(finding: Finding, lines: &[&str]) -> Diagnostic {
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
        source: Some("rpmcrab".to_string()),
        message,
        ..Default::default()
    }
}
