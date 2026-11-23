//! Renaming a subpackage: its `%package`, `%description`, `%files` and
//! scriptlets, in their short form or with `-n`, and the dependencies of the
//! spec on it.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Result, bail};
use lsp_types::{Position, PrepareRenameResponse, TextEdit, WorkspaceEdit};

use crate::document::Document;
use crate::links;
use crate::packages::Packages;

/// The name of a section of a subpackage.
struct Header {
    full: String,
    bytes: Range<usize>,
    /// Without `-n`: the name after the package's, like `libs` of `foo-libs`.
    short: bool,
}

/// The names of the sections of the subpackages, up to the first blank,
/// like `%{name}-x` of several nodes, before options like `-f files.list`.
fn headers(doc: &Document, main: &str, values: &HashMap<String, String>) -> Vec<Header> {
    let mut headers = Vec::new();
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit
            && crate::outline::section_kind(node.kind()).is_some()
            && let Some(name) = node.child_by_field_name("name")
        {
            // The name of %description starts with the blank before it.
            let rest = &doc.text[name.start_byte()..];
            let start = name.start_byte() + rest.len() - rest.trim_start().len();
            let len = doc.text[start..].find(char::is_whitespace).unwrap_or(doc.text.len() - start);
            let bytes = start..start + len;
            let short = !doc.text[node.start_byte()..start].split_whitespace().any(|w| w == "-n");
            if let Some(name) = links::expand(&doc.text[bytes.clone()], values, 0) {
                let full = if short { format!("{main}-{name}") } else { name };
                headers.push(Header { full, bytes, short });
            }
        }
        if visit && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
        } else if cursor.goto_parent() {
            visit = false;
        } else {
            return headers;
        }
    }
}

/// The subpackage at `position`: a section's name, or a dependency on one
/// of the spec, with its range, and whether it is in the short form.
struct Target {
    full: String,
    bytes: Range<usize>,
    short: bool,
    main: String,
    headers: Vec<Header>,
}

fn target(doc: &Document, position: Position) -> Option<Target> {
    let values = links::macro_values(doc);
    let main = links::expand(values.get("name")?, &values, 0)?;
    let headers = headers(doc, &main, &values);
    let offset = doc.offset(position);
    let within = |bytes: &Range<usize>| bytes.start <= offset && offset <= bytes.end;
    let (full, bytes, short) = match headers.iter().find(|h| within(&h.bytes)) {
        Some(h) => (h.full.clone(), h.bytes.clone(), h.short),
        None => {
            let packages = Packages::of(doc);
            let (name, range) =
                packages.required.iter().find(|(_, r)| r.start <= position && position <= r.end)?;
            if !headers.iter().any(|h| &h.full == name) {
                return None;
            }
            (name.clone(), doc.offset(range.start)..doc.offset(range.end), false)
        }
    };
    Some(Target { full, bytes, short, main, headers })
}

pub fn prepare_rename(doc: &Document, position: Position) -> Option<PrepareRenameResponse> {
    let target = target(doc, position)?;
    Some(PrepareRenameResponse::Range(doc.range(target.bytes)))
}

/// Rename the subpackage at `position` to `new_name`, of the form renamed:
/// short in a short section name, else full.
pub fn rename(doc: &Document, position: Position, new_name: &str) -> Result<Option<WorkspaceEdit>> {
    let Some(target) = target(doc, position) else { return Ok(None) };
    if new_name.is_empty() || new_name.contains(char::is_whitespace) {
        bail!("invalid package name: {new_name}");
    }
    let prefix = format!("{}-", target.main);
    let (new_full, new_short) = if target.short {
        (format!("{prefix}{new_name}"), Some(new_name.to_string()))
    } else {
        (new_name.to_string(), new_name.strip_prefix(&prefix).map(str::to_string))
    };
    let old_short = target.full.strip_prefix(&prefix).map(str::to_string);
    let mut edits = Vec::new();
    for header in target.headers.iter().filter(|h| h.full == target.full) {
        let text = match (&new_short, header.short) {
            (Some(short), true) => short.clone(),
            (None, true) => format!("-n {new_full}"),
            (_, false) => new_full.clone(),
        };
        edits.push(TextEdit::new(doc.range(header.bytes.clone()), text));
    }
    // The dependencies: the short part only, at the end, like `%{name}-libs`.
    for (_, range) in Packages::of(doc).required.into_iter().filter(|(n, _)| *n == target.full) {
        let bytes = doc.offset(range.start)..doc.offset(range.end);
        let text = &doc.text[bytes.clone()];
        let edit = match (&old_short, &new_short) {
            (Some(old), Some(new)) if text.ends_with(&format!("-{old}")) => {
                let start = bytes.end - old.len();
                TextEdit::new(doc.range(start..bytes.end), new.clone())
            }
            _ => TextEdit::new(range, new_full.clone()),
        };
        edits.push(edit);
    }
    Ok(Some(WorkspaceEdit::new(HashMap::from([(doc.uri.clone(), edits)]))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;

    const SPEC: &str = "\
Name: foo
Requires: %{name}-libs = 1
Requires: foo-libs
%package libs
Summary: s
%description libs
d
%files libs -f libs.list
%post -n foo-libs
true
%package -n python3-foo
Summary: p
%files -n python3-foo
";

    fn applied(d: &Document, edit: WorkspaceEdit) -> String {
        let mut edits = edit.changes.unwrap().remove(&d.uri).unwrap();
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        let mut text = d.text.clone();
        for e in edits {
            text.replace_range(d.offset(e.range.start)..d.offset(e.range.end), &e.new_text);
        }
        text
    }

    #[test]
    fn subpackages_rename_with_their_sections_and_dependencies() {
        let d = doc(SPEC);
        // From a short section name: the short forms and the full ones.
        let edit = rename(&d, Position::new(3, 10), "core").unwrap().unwrap();
        assert_eq!(
            applied(&d, edit),
            SPEC.replace("%{name}-libs", "%{name}-core")
                .replace("foo-libs", "foo-core")
                .replace("libs\n", "core\n")
                .replace("libs -f", "core -f")
        );
        // From a dependency, to a name out of the package's: with -n.
        let edit = rename(&d, Position::new(2, 12), "libfoo").unwrap().unwrap();
        let text = applied(&d, edit);
        assert!(text.contains("%package -n libfoo\n"), "{text}");
        assert!(text.contains("%files -n libfoo -f libs.list\n%post -n libfoo\n"), "{text}");
        assert!(text.contains("Requires: libfoo = 1\nRequires: libfoo\n"), "{text}");
        // Only subpackages of the spec.
        assert!(prepare_rename(&d, Position::new(0, 7)).is_none());
        assert!(prepare_rename(&d, Position::new(12, 12)).is_some());
    }
}
