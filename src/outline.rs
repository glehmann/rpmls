//! Document symbols (the outline) and folding ranges.

use lsp_types::{DocumentSymbol, FoldingRange, FoldingRangeKind, Position, Range, SymbolKind};
use tree_sitter::Node;

use crate::document::Document;

/// The symbol kind of a section node, or `None` when the node is no section.
pub fn section_kind(kind: &str) -> Option<SymbolKind> {
    Some(match kind {
        "package" => SymbolKind::PACKAGE,
        "description" => SymbolKind::STRING,
        "files" | "sourcelist" | "patchlist" => SymbolKind::FILE,
        "changelog" => SymbolKind::EVENT,
        "prep_scriptlet"
        | "build_scriptlet"
        | "install_scriptlet"
        | "check_scriptlet"
        | "clean_scriptlet"
        | "conf_scriptlet"
        | "generate_buildrequires"
        | "runtime_scriptlet"
        | "runtime_scriptlet_interpreter"
        | "trigger"
        | "file_trigger" => SymbolKind::FUNCTION,
        _ => return None,
    })
}

/// The sections, each holding the macros it defines, and the macros defined
/// outside any section. Conditionals are looked through.
pub fn symbols(doc: &Document) -> Vec<DocumentSymbol> {
    collect(doc, doc.tree.root_node())
}

fn collect(doc: &Document, node: Node) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(kind) = section_kind(child.kind()) {
            let header = doc.node_text(child).lines().next().unwrap_or_default().trim();
            symbols.push(symbol(doc, child, header.to_string(), None, kind, collect(doc, child)));
        } else if child.kind() == "macro_definition"
            && let Some(name) = child.child_by_field_name("name")
        {
            let value =
                doc.text[name.end_byte()..child.end_byte()].lines().next().unwrap_or_default();
            let detail = Some(value.trim().to_string()).filter(|v| !v.is_empty());
            let name = format!("%{}", doc.node_text(name));
            symbols.push(symbol(doc, child, name, detail, SymbolKind::CONSTANT, Vec::new()));
        } else {
            symbols.extend(collect(doc, child));
        }
    }
    symbols
}

fn symbol(
    doc: &Document,
    node: Node,
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    let start = node.start_position().row;
    let end = last_line(doc, node);
    let header = Range::new(
        doc.position(node.start_byte()),
        Position::new(start as u32, doc.line(start).encode_utf16().count() as u32),
    );
    #[allow(deprecated)]
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range: Range::new(
            header.start,
            Position::new(end as u32, doc.line(end).encode_utf16().count() as u32),
        ),
        selection_range: header,
        children: Some(children).filter(|c| !c.is_empty()),
    }
}

/// The node's last non-blank line: a node ending at a line start ends on the
/// line before, and trailing blank lines are not part of a section.
fn last_line(doc: &Document, node: Node) -> usize {
    let start = node.start_position().row;
    let end = node.end_position();
    let mut line = if end.column == 0 && end.row > start { end.row - 1 } else { end.row };
    while line > start && doc.line(line).trim().is_empty() {
        line -= 1;
    }
    line
}

/// Sections, conditionals, changelog entries and multi-line macro
/// definitions fold as regions; runs of comment lines fold as comments.
pub fn folding_ranges(doc: &Document) -> Vec<FoldingRange> {
    let mut ranges = Vec::new();
    let mut comments: Option<(usize, usize)> = None;
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit {
            if node.kind() == "comment" {
                let line = node.start_position().row;
                comments = match comments {
                    Some((first, last)) if last + 1 == line => Some((first, line)),
                    run => {
                        push_comments(&mut ranges, run);
                        Some((line, line))
                    }
                };
            } else if section_kind(node.kind()).is_some()
                || matches!(
                    node.kind(),
                    "if_statement"
                        | "ifarch_statement"
                        | "ifos_statement"
                        | "changelog_entry"
                        | "macro_definition"
                )
            {
                let (start, end) = (node.start_position().row, last_line(doc, node));
                if end > start {
                    ranges.push(FoldingRange {
                        start_line: start as u32,
                        end_line: end as u32,
                        kind: Some(FoldingRangeKind::Region),
                        ..Default::default()
                    });
                }
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
            break;
        }
    }
    push_comments(&mut ranges, comments);
    ranges
}

fn push_comments(ranges: &mut Vec<FoldingRange>, run: Option<(usize, usize)>) {
    if let Some((first, last)) = run.filter(|(first, last)| last > first) {
        ranges.push(FoldingRange {
            start_line: first as u32,
            end_line: last as u32,
            kind: Some(FoldingRangeKind::Comment),
            ..Default::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;

    const SPEC: &str = "\
# A comment
# on two lines
%global srcname foo
Name: %{srcname}

%description
Text.

%package devel
Summary: Devel

%prep
%autosetup

%if 0%{?with_x}
%files devel
/usr/include/foo.h
%define inner 1
%endif

%changelog
* Mon Jan 01 2024 A <a@b> - 1-1
- init
";

    fn outline(symbols: &[DocumentSymbol]) -> Vec<String> {
        symbols
            .iter()
            .flat_map(|s| {
                let mut out =
                    vec![format!("{} {}-{}", s.name, s.range.start.line, s.range.end.line)];
                out.extend(
                    outline(s.children.as_deref().unwrap_or_default())
                        .iter()
                        .map(|c| format!("  {c}")),
                );
                out
            })
            .collect()
    }

    #[test]
    fn symbols_list_sections_and_macros() {
        assert_eq!(
            outline(&symbols(&doc(SPEC))),
            [
                "%srcname 2-2",
                "%description 5-6",
                "%package devel 8-9",
                "%prep 11-12",
                "%files devel 15-17",
                "  %inner 17-17",
                "%changelog 20-22",
            ]
        );
    }

    #[test]
    fn folding_covers_sections_conditionals_and_comments() {
        let mut ranges: Vec<_> = folding_ranges(&doc(SPEC))
            .iter()
            .map(|r| (r.start_line, r.end_line, r.kind.clone() == Some(FoldingRangeKind::Comment)))
            .collect();
        ranges.sort();
        assert_eq!(
            ranges,
            [
                (0, 1, true),
                (5, 6, false),
                (8, 9, false),
                (11, 12, false),
                (14, 18, false),
                (15, 17, false),
                (20, 22, false),
                (21, 22, false),
            ]
        );
    }
}
