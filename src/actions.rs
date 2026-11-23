//! Code actions: add a changelog entry, and suppress a finding in the
//! rpmlintrc.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, Position, Range,
    TextEdit, WorkspaceEdit,
};

use crate::document::Document;
use crate::links;

/// The actions on `doc`, the findings suppressed in `rpmlintrc` when given.
pub fn actions(
    doc: &Document,
    diagnostics: &[Diagnostic],
    rpmlintrc: Option<&Path>,
) -> Vec<CodeActionOrCommand> {
    let mut actions: Vec<CodeActionOrCommand> = diagnostics
        .iter()
        .filter_map(|d| suppress(doc, d, rpmlintrc))
        .map(CodeActionOrCommand::CodeAction)
        .collect();
    actions.extend(
        changelog_entry(doc, &today(), &packager(doc)).map(CodeActionOrCommand::CodeAction),
    );
    actions
}

/// Insert a changelog entry for the spec's EVR at the top of `%changelog`,
/// unless rpmautospec writes the changelog.
fn changelog_entry(doc: &Document, date: &str, packager: &str) -> Option<CodeAction> {
    let header = (0..doc.line_count()).find(|&i| doc.line(i).trim_end() == "%changelog")?;
    let next = (header + 1..doc.line_count()).map(|i| doc.line(i).trim()).find(|l| !l.is_empty());
    if next == Some("%autochangelog") {
        return None;
    }
    let evr = evr(doc).map(|evr| format!(" - {evr}")).unwrap_or_default();
    let separator = if next.is_some() { "\n" } else { "" };
    let text = format!("* {date} {packager}{evr}\n- \n{separator}");
    let at = Position::new(header as u32 + 1, 0);
    Some(CodeAction {
        title: "Add a changelog entry".into(),
        kind: Some(CodeActionKind::SOURCE),
        edit: Some(WorkspaceEdit::new(HashMap::from([(
            doc.uri.clone(),
            vec![TextEdit::new(Range::new(at, at), text)],
        )]))),
        ..Default::default()
    })
}

/// `[epoch:]version-release`, with the spec's simple macros substituted, or
/// `None` when they are not enough, as with `%autorelease`.
fn evr(doc: &Document) -> Option<String> {
    let values = links::macro_values(doc);
    let get = |tag: &str| links::expand(values.get(tag)?, &values, 0);
    let epoch = values.get("epoch").map(|_| get("epoch")).unwrap_or(Some(String::new()))?;
    let epoch = if epoch.is_empty() { String::new() } else { format!("{epoch}:") };
    Some(format!("{epoch}{}-{}", get("version")?, get("release")?))
}

/// rpm's `%packager`, else git's user, as `Name <email>`.
fn packager(doc: &Document) -> String {
    let output = |cmd: &mut Command| {
        cmd.output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(packager) = output(Command::new("rpm").args(["--eval", "%{?packager}"])) {
        return packager;
    }
    let git = |key| {
        let mut cmd = Command::new("git");
        if let Some(dir) = links::doc_dir(doc) {
            cmd.current_dir(dir);
        }
        output(cmd.args(["config", key]))
    };
    match (git("user.name"), git("user.email")) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (Some(name), None) => name,
        _ => "Packager <email>".into(),
    }
}

/// Today, as a changelog date: `Mon Jan 01 2024`.
fn today() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    changelog_date(secs / 86400)
}

fn changelog_date(days: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{} {} {day:02} {year}", WEEKDAYS[(days % 7) as usize], MONTHS[month as usize - 1])
}

/// The command suppressing a finding, of the rpmlintrc and the filter to
/// add to it: rpmls writes it, rather than the editor, which would leave it
/// in a buffer to save, and lints the specs again.
pub const SUPPRESS: &str = "rpmls.suppress";

/// Suppress a finding with an `addFilter` in `rpmlintrc`, or else in the
/// rpmlintrc the linter loads beside the spec: `<spec name>.rpmlintrc`, else
/// the first it loads, created when there is none.
fn suppress(
    doc: &Document,
    diagnostic: &Diagnostic,
    rpmlintrc: Option<&Path>,
) -> Option<CodeAction> {
    if !matches!(diagnostic.source.as_deref(), Some("rpmcrab" | "rpmlint")) {
        return None;
    }
    let Some(NumberOrString::String(check)) = &diagnostic.code else { return None };
    let spec = links::doc_path(doc)?;
    let dir = spec.parent()?;
    let own = dir.join(format!("{}.rpmlintrc", spec.file_stem()?.to_string_lossy()));
    let path = match rpmlintrc {
        Some(path) => path.to_path_buf(),
        None if own.is_file() => own,
        None => crate::lint::rpmlintrc_files(dir).into_iter().next().unwrap_or(own),
    };
    let title = format!("Suppress {check} in {}", path.file_name()?.to_string_lossy());
    let filter = format!("addFilter(\"{}\")", regex_escape(check));
    let arguments = vec![path.to_string_lossy().into(), filter.into()];
    Some(CodeAction {
        title: title.clone(),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        command: Some(lsp_types::Command::new(title, SUPPRESS.into(), Some(arguments))),
        ..Default::default()
    })
}

/// Append the line `filter` to the rpmlintrc `path`, created when missing.
pub fn add_filter(path: &Path, filter: &str) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let separator = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(format!("{separator}{filter}\n").as_bytes()))
        .with_context(|| format!("adding {filter} to {}", path.display()))
}

/// `addFilter` takes a regular expression.
fn regex_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let special = "\\.+*?()|[]{}^$".contains(c);
            special.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::{doc, parser};

    #[test]
    fn changelog_dates_follow_the_calendar() {
        assert_eq!(changelog_date(0), "Thu Jan 01 1970");
        assert_eq!(changelog_date(19_723), "Mon Jan 01 2024");
        assert_eq!(changelog_date(20_416), "Mon Nov 24 2025");
        assert_eq!(changelog_date(19_782), "Thu Feb 29 2024");
    }

    #[test]
    fn changelog_entry_carries_the_evr() {
        let d = doc(
            "%global rel 3\nName: foo\nEpoch: 1\nVersion: 2.0\nRelease: %{rel}%{?dist}\n%changelog\n* Old entry\n- x\n",
        );
        let action = changelog_entry(&d, "Mon Jan 01 2024", "A <a@b>").unwrap();
        let edits = &action.edit.unwrap().changes.unwrap()[&d.uri];
        assert_eq!(edits[0].range.start, Position::new(6, 0));
        assert_eq!(edits[0].new_text, "* Mon Jan 01 2024 A <a@b> - 1:2.0-3\n- \n\n");
        // rpmautospec writes the changelog.
        assert!(
            changelog_entry(&doc("Name: foo\n%changelog\n%autochangelog\n"), "d", "p").is_none()
        );
        // No EVR with %autorelease.
        let d = doc("Name: foo\nVersion: 1\nRelease: %autorelease\n%changelog\n");
        let action = changelog_entry(&d, "D", "P").unwrap();
        assert_eq!(action.edit.unwrap().changes.unwrap()[&d.uri][0].new_text, "* D P\n- \n");
    }

    #[test]
    fn suppress_appends_to_or_creates_the_rpmlintrc() {
        let dir = std::env::temp_dir().join(format!("rpmls-actions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let uri = links::file_uri(&dir.join("foo.spec")).unwrap();
        let d = Document::new(&mut parser(), uri, 1, "Name: foo\n".into()).unwrap();
        let diagnostic = Diagnostic {
            source: Some("rpmlint".into()),
            code: Some(NumberOrString::String("no-%build-section".into())),
            ..Default::default()
        };
        let suppress = |rpmlintrc: Option<&Path>| {
            let action = suppress(&d, &diagnostic, rpmlintrc).unwrap();
            let command = action.command.unwrap();
            let args = command.arguments.unwrap();
            (
                action.title,
                args[0].as_str().unwrap().to_string(),
                args[1].as_str().unwrap().to_string(),
            )
        };
        let (title, path, filter) = suppress(None);
        assert_eq!(title, "Suppress no-%build-section in foo.rpmlintrc");
        assert_eq!(path, dir.join("foo.rpmlintrc").to_string_lossy());
        assert_eq!(filter, "addFilter(\"no-%build-section\")");
        // Created, then appended to, after a last line without its end.
        add_filter(Path::new(&path), &filter).unwrap();
        std::fs::write(&path, "addFilter(\"a\")").unwrap();
        add_filter(Path::new(&path), &filter).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "addFilter(\"a\")\naddFilter(\"no-%build-section\")\n"
        );
        // In the project's rpmlintrc, when given.
        let project = dir.join("lint/foo.rpmlintrc");
        assert_eq!(suppress(Some(&project)).1, project.to_string_lossy());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(regex_escape("a.b+c"), "a\\.b\\+c");
    }
}
