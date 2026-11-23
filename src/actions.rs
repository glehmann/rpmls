//! Code actions: add a changelog entry, and suppress a finding in the
//! rpmlintrc.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CreateFile, CreateFileOptions, Diagnostic,
    DocumentChangeOperation, DocumentChanges, NumberOrString, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, TextDocumentEdit,
    TextEdit, WorkspaceEdit,
};

use crate::document::Document;
use crate::links;

pub fn actions(doc: &Document, diagnostics: &[Diagnostic]) -> Vec<CodeActionOrCommand> {
    let mut actions: Vec<CodeActionOrCommand> = diagnostics
        .iter()
        .filter_map(|d| suppress(doc, d))
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

/// Suppress an rpmcrab finding with an `addFilter` in the rpmlintrc rpmcrab
/// loads beside the spec, created when there is none.
fn suppress(doc: &Document, diagnostic: &Diagnostic) -> Option<CodeAction> {
    if diagnostic.source.as_deref() != Some("rpmcrab") {
        return None;
    }
    let Some(NumberOrString::String(check)) = &diagnostic.code else { return None };
    let dir = links::doc_dir(doc)?;
    let spec = url::Url::parse(doc.uri.as_str()).ok()?.to_file_path().ok()?;
    let stem = spec.file_stem()?.to_string_lossy().into_owned();
    let line = format!("addFilter(\"{}\")\n", regex_escape(check));
    let (path, existing) = match rpmlintrc(&dir, &stem) {
        Some(path) => {
            let text = std::fs::read_to_string(&path).ok()?;
            (path, Some(text))
        }
        None => (dir.join(format!("{stem}.rpmlintrc")), None),
    };
    let uri = links::file_uri(&path)?;
    let (at, text) = match &existing {
        Some(text) => {
            let lines = text.lines().count() as u32;
            match text.lines().last() {
                Some(last) if !text.ends_with('\n') => (
                    Position::new(lines - 1, last.encode_utf16().count() as u32),
                    format!("\n{line}"),
                ),
                _ => (Position::new(lines, 0), line),
            }
        }
        None => (Position::new(0, 0), line),
    };
    let edit = DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier { uri: uri.clone(), version: None },
        edits: vec![OneOf::Left(TextEdit::new(Range::new(at, at), text))],
    });
    let mut operations = Vec::new();
    if existing.is_none() {
        operations.push(DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
            uri,
            options: Some(CreateFileOptions {
                overwrite: Some(false),
                ignore_if_exists: Some(true),
            }),
            annotation_id: None,
        })));
    }
    operations.push(edit);
    let name = path.file_name()?.to_string_lossy();
    Some(CodeAction {
        title: format!("Suppress {check} in {name}"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(operations)),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// The rpmlintrc to add filters to: `<spec name>.rpmlintrc`, else the first
/// rpmcrab loads from the spec's directory (`*.rpmlintrc`, `*-rpmlintrc`).
fn rpmlintrc(dir: &Path, stem: &str) -> Option<PathBuf> {
    let own = dir.join(format!("{stem}.rpmlintrc"));
    if own.is_file() {
        return Some(own);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    [".rpmlintrc", "-rpmlintrc"]
        .iter()
        .find_map(|suffix| files.iter().find(|p| p.to_string_lossy().ends_with(suffix)).cloned())
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
            source: Some("rpmcrab".into()),
            code: Some(NumberOrString::String("no-%build-section".into())),
            ..Default::default()
        };
        let operations = |action: CodeAction| match action.edit.unwrap().document_changes.unwrap() {
            DocumentChanges::Operations(ops) => ops,
            DocumentChanges::Edits(_) => panic!(),
        };
        let action = suppress(&d, &diagnostic).unwrap();
        assert_eq!(action.title, "Suppress no-%build-section in foo.rpmlintrc");
        let ops = operations(action);
        assert!(matches!(ops[0], DocumentChangeOperation::Op(ResourceOp::Create(_))));
        let DocumentChangeOperation::Edit(edit) = &ops[1] else { panic!() };
        let OneOf::Left(text) = &edit.edits[0] else { panic!() };
        assert_eq!(text.new_text, "addFilter(\"no-%build-section\")\n");

        std::fs::write(dir.join("foo.rpmlintrc"), "addFilter(\"a\")").unwrap();
        let ops = operations(suppress(&d, &diagnostic).unwrap());
        assert_eq!(ops.len(), 1);
        let DocumentChangeOperation::Edit(edit) = &ops[0] else { panic!() };
        let OneOf::Left(text) = &edit.edits[0] else { panic!() };
        assert_eq!(text.range.start, Position::new(0, 14));
        assert_eq!(text.new_text, "\naddFilter(\"no-%build-section\")\n");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(regex_escape("a.b+c"), "a\\.b\\+c");
    }
}
