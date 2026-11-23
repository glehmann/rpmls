//! The packages a spec builds, and those it depends on, to navigate between
//! the specs of the workspace by package name.

use std::collections::HashMap;

use lsp_types::{Position, Range, Uri};
use tree_sitter::Node;

use crate::document::Document;
use crate::links;

/// The package names of a spec, with their ranges.
#[derive(Clone)]
pub struct Packages {
    pub uri: Uri,
    /// Those it builds: its `Name`, its subpackages, and its `Provides`.
    pub built: Vec<(String, Range)>,
    /// Those it depends on: its other dependency tags, but `Obsoletes` and
    /// `Conflicts`.
    pub required: Vec<(String, Range)>,
}

impl Packages {
    /// The packages of `doc`, their macros substituted when they are simple,
    /// like `%{name}-libs`; a name with another macro is left out.
    pub fn of(doc: &Document) -> Self {
        let values = links::macro_values(doc);
        let expand = |node: Node| links::expand(doc.node_text(node), &values, 0);
        let main = values.get("name").and_then(|name| links::expand(name, &values, 0));
        let mut packages = Self { uri: doc.uri.clone(), built: Vec::new(), required: Vec::new() };
        let mut cursor = doc.tree.walk();
        let mut visit = true;
        loop {
            let node = cursor.node();
            if visit {
                packages.add(doc, node, main.as_deref(), &values, &expand);
            }
            if visit && cursor.goto_first_child() {
                continue;
            }
            if cursor.goto_next_sibling() {
                visit = true;
            } else if cursor.goto_parent() {
                visit = false;
            } else {
                return packages;
            }
        }
    }

    fn add(
        &mut self,
        doc: &Document,
        node: Node,
        main: Option<&str>,
        values: &HashMap<String, String>,
        expand: &impl Fn(Node) -> Option<String>,
    ) {
        match node.kind() {
            "preamble_tag" => {
                let Some(tag) = node.named_child(0).filter(|t| t.kind() == "tag") else { return };
                if doc.node_text(tag).trim_end_matches(':').eq_ignore_ascii_case("name")
                    && let Some(value) = node.child_by_field_name("value")
                    && let Some(name) = expand(value)
                {
                    self.built.push((name, doc.range(value.byte_range())));
                }
            }
            "package" => {
                let (Some(name), Some(main)) = (node.child_by_field_name("name"), main) else {
                    return;
                };
                let header = &doc.text[node.start_byte()..name.start_byte()];
                // The name runs to the end of the line, like %{name}-libs, of several nodes.
                let line_end = doc.text[name.start_byte()..]
                    .find('\n')
                    .map_or(doc.text.len(), |i| i + name.start_byte());
                let bytes = name.start_byte()..line_end;
                let Some(sub) = links::expand(doc.text[bytes.clone()].trim_end(), values, 0) else {
                    return;
                };
                let full = if header.split_whitespace().any(|w| w == "-n") {
                    sub
                } else {
                    format!("{main}-{sub}")
                };
                self.built.push((full, doc.range(bytes)));
            }
            "dependency_tag" => {
                let tag = doc.node_text(node).to_lowercase();
                let tag = tag.trim_end_matches(':');
                let list = if tag.starts_with("provides") {
                    &mut self.built
                } else if tag.starts_with("obsoletes") || tag.contains("conflicts") {
                    return;
                } else {
                    &mut self.required
                };
                let Some(parent) = node.parent() else { return };
                let mut cursor = parent.walk();
                for value in parent.children_by_field_name("value", &mut cursor) {
                    let mut names = value.walk();
                    let names: Vec<Node> =
                        value.children_by_field_name("name", &mut names).collect();
                    let (Some(first), Some(last)) = (names.first(), names.last()) else { continue };
                    let bytes = first.start_byte()..last.end_byte();
                    if let Some(name) = links::expand(&doc.text[bytes.clone()], values, 0) {
                        list.push((name, doc.range(bytes)));
                    }
                }
            }
            _ => {}
        }
    }

    /// The package name at `position`, and whether it is a dependency.
    pub fn at(&self, position: Position) -> Option<(&str, bool)> {
        let within = |range: &Range| range.start <= position && position <= range.end;
        let built = self.built.iter().find(|(_, r)| within(r)).map(|(n, _)| (n.as_str(), false));
        built.or_else(|| {
            self.required.iter().find(|(_, r)| within(r)).map(|(n, _)| (n.as_str(), true))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;

    #[test]
    fn packages_are_those_built_and_required() {
        let d = doc("\
%global subname libs
Name: foo
Provides: foo-compat = 1
BuildRequires: bar-devel >= 2, baz, qux-devel
Obsoletes: old
Requires: %{name}-%{subname} = 1
Requires(post): %{unknown}-x
%package %{subname}
Summary: s
Requires(post): qux
%package -n python3-foo
Summary: p
%package -n %{name}-x
Summary: x
");
        let p = Packages::of(&d);
        let names = |list: &[(String, Range)]| -> Vec<String> {
            list.iter().map(|(n, _)| n.clone()).collect()
        };
        assert_eq!(names(&p.built), ["foo", "foo-compat", "foo-libs", "python3-foo", "foo-x"]);
        assert_eq!(names(&p.required), ["bar-devel", "baz", "qux-devel", "foo-libs", "qux"]);
        let at = |line, character| p.at(Position::new(line, character));
        assert_eq!(at(3, 17), Some(("bar-devel", true)));
        assert_eq!(at(7, 10), Some(("foo-libs", false)));
        assert_eq!(at(4, 12), None);
    }
}
