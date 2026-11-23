//! Macro navigation: definitions, references, highlights and rename.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Result, bail};
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, Location, Position, PrepareRenameResponse, TextEdit,
    WorkspaceEdit,
};
use tree_sitter::Node;

use crate::document::Document;

/// A macro name in the document: the name alone, without `%`, braces or
/// `?`/`!` operators.
#[derive(Debug, PartialEq)]
pub struct Occurrence {
    pub name: String,
    pub bytes: Range<usize>,
    /// Whether this is a `%define`/`%global` of the macro.
    pub definition: bool,
}

/// Every macro name in the document, in document order.
pub fn occurrences(doc: &Document) -> Vec<Occurrence> {
    let mut found = Vec::new();
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        if visit {
            found.extend(occurrence(doc, cursor.node()));
        }
        if visit && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
        } else if cursor.goto_parent() {
            visit = false;
        } else {
            return found;
        }
    }
}

fn occurrence(doc: &Document, node: Node) -> Option<Occurrence> {
    let parent = node.parent()?;
    let field = |name| parent.child_by_field_name(name) == Some(node);
    let (definition, bytes) = match (node.kind(), parent.kind()) {
        ("identifier", "macro_definition") if field("name") => (true, node.byte_range()),
        ("identifier", "macro_undefinition" | "macro_parametric_expansion") if field("name") => {
            (false, node.byte_range())
        }
        ("identifier", "conditional_expansion") if field("condition") => (false, node.byte_range()),
        // %{name} and %{name args}: the name is the first child after `%{`.
        ("identifier", "macro_expansion") if parent.named_child(0) == Some(node) => {
            (false, node.byte_range())
        }
        ("simple_macro", _) => (false, node.byte_range()),
        ("negated_macro", _) => (false, node.start_byte() + 1..node.end_byte()),
        _ => return None,
    };
    let name = doc.text.get(bytes.clone())?.to_string();
    Some(Occurrence { name, bytes, definition })
}

/// The occurrence under the cursor, if any; the cursor may sit right after
/// the name.
fn at(doc: &Document, position: Position) -> Option<Occurrence> {
    let offset = doc.offset(position);
    occurrences(doc).into_iter().find(|o| o.bytes.start <= offset && offset <= o.bytes.end)
}

/// The name of the macro under the cursor, if any.
pub fn name_at(doc: &Document, position: Position) -> Option<String> {
    at(doc, position).map(|o| o.name)
}

fn location(doc: &Document, bytes: &Range<usize>) -> Location {
    Location::new(doc.uri.clone(), doc.range(bytes.clone()))
}

pub fn definitions(doc: &Document, position: Position) -> Vec<Location> {
    let Some(target) = at(doc, position) else { return Vec::new() };
    occurrences(doc)
        .iter()
        .filter(|o| o.definition && o.name == target.name)
        .map(|o| location(doc, &o.bytes))
        .collect()
}

pub fn references(doc: &Document, position: Position, include_declaration: bool) -> Vec<Location> {
    let Some(target) = at(doc, position) else { return Vec::new() };
    occurrences(doc)
        .iter()
        .filter(|o| o.name == target.name && (include_declaration || !o.definition))
        .map(|o| location(doc, &o.bytes))
        .collect()
}

pub fn highlights(doc: &Document, position: Position) -> Vec<DocumentHighlight> {
    let Some(target) = at(doc, position) else { return Vec::new() };
    occurrences(doc)
        .iter()
        .filter(|o| o.name == target.name)
        .map(|o| DocumentHighlight {
            range: doc.range(o.bytes.clone()),
            kind: Some(if o.definition {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            }),
        })
        .collect()
}

/// Only macros defined in the document can be renamed: renaming a use of a
/// system macro like `%{_bindir}` would break the spec.
pub fn prepare_rename(doc: &Document, position: Position) -> Option<PrepareRenameResponse> {
    let target = at(doc, position)?;
    occurrences(doc)
        .iter()
        .any(|o| o.definition && o.name == target.name)
        .then(|| PrepareRenameResponse::Range(doc.range(target.bytes)))
}

pub fn rename(doc: &Document, position: Position, new_name: &str) -> Result<Option<WorkspaceEdit>> {
    let new_name = new_name.trim_start_matches('%');
    let mut chars = new_name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("invalid macro name: {new_name}");
    }
    if prepare_rename(doc, position).is_none() {
        return Ok(None);
    }
    let Some(target) = at(doc, position) else { return Ok(None) };
    let edits = occurrences(doc)
        .iter()
        .filter(|o| o.name == target.name)
        .map(|o| TextEdit::new(doc.range(o.bytes.clone()), new_name.to_string()))
        .collect();
    Ok(Some(WorkspaceEdit::new(HashMap::from([(doc.uri.clone(), edits)]))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;

    const SPEC: &str = "\
%global srcname foo
%{!?with_x: %global with_x 0}
Name: %{srcname}
URL: https://example.org/%srcname
%if %{with_x}
Requires: %{srcname}-x
%endif

%description
%{srcname} demo.

%build
%make_build NAME=%!srcname
%{?srcname:echo yes}
%undefine srcname
";

    fn names(doc: &Document) -> Vec<(String, String, bool)> {
        occurrences(doc)
            .into_iter()
            .map(|o| (o.name, doc.text[o.bytes].to_string(), o.definition))
            .collect()
    }

    #[test]
    fn occurrences_cover_every_macro_form() {
        let d = doc(SPEC);
        let found = names(&d);
        let srcname: Vec<_> = found.iter().filter(|(n, ..)| n == "srcname").collect();
        assert_eq!(srcname.len(), 8, "{found:?}");
        assert!(srcname.iter().all(|(n, text, _)| n == text));
        assert_eq!(srcname.iter().filter(|(.., def)| *def).count(), 1);
        assert!(found.contains(&("with_x".into(), "with_x".into(), true)));
        assert!(found.contains(&("make_build".into(), "make_build".into(), false)));
    }

    #[test]
    fn navigation_finds_definitions_and_references() {
        let d = doc(SPEC);
        // On `srcname` in `Name: %{srcname}`.
        let at = Position::new(2, 10);
        let defs = definitions(&d, at);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].range.start, Position::new(0, 8));
        assert_eq!(references(&d, at, true).len(), 8);
        assert_eq!(references(&d, at, false).len(), 7);
        let writes = highlights(&d, at)
            .iter()
            .filter(|h| h.kind == Some(DocumentHighlightKind::WRITE))
            .count();
        assert_eq!(writes, 1);
        // Nothing under the cursor in plain text.
        assert!(definitions(&d, Position::new(9, 13)).is_empty());
    }

    #[test]
    fn rename_only_touches_local_macros() {
        let d = doc(SPEC);
        let edit = rename(&d, Position::new(2, 10), "%pkgname").unwrap().unwrap();
        let edits = &edit.changes.unwrap()[&d.uri];
        assert_eq!(edits.len(), 8);
        assert!(edits.iter().all(|e| e.new_text == "pkgname"));
        // %make_build is not defined in the spec.
        assert!(prepare_rename(&d, Position::new(12, 3)).is_none());
        assert!(rename(&d, Position::new(2, 10), "bad name").is_err());
    }
}
