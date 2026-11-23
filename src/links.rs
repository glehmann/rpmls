//! Document links: sources and patches to their local files, URLs to the web.
//!
//! Link targets often hold macros. rpm never expands them (that could run the
//! spec's code): only the spec's own simple macros and tag values are
//! substituted, and a target with any other macro gets no link.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{DocumentLink, Uri};
use tree_sitter::Node;

use crate::document::Document;
use crate::macros;

/// The tags rpm also defines as macros, like `%{name}` and `%{url}`.
const TAG_MACROS: &[&str] = &["name", "version", "release", "epoch", "summary", "license", "url"];

pub fn links(doc: &Document) -> Vec<DocumentLink> {
    let values = macro_values(doc);
    let dir = doc_dir(doc);
    let mut links = Vec::new();
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit && let Some((value, local)) = link_value(doc, node) {
            let target = expand(doc.node_text(value), &values, 0).and_then(|text| {
                let local_file = local.then(|| local_file(dir.as_deref()?, &text)).flatten();
                local_file.or_else(|| web(&text))
            });
            if let Some(target) = target {
                links.push(DocumentLink {
                    range: doc.range(value.byte_range()),
                    target: Some(target),
                    tooltip: None,
                    data: None,
                });
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
            return links;
        }
    }
}

/// The value to link of a node, and whether it names a source or patch file.
fn link_value<'t>(doc: &Document, node: Node<'t>) -> Option<(Node<'t>, bool)> {
    match node.kind() {
        "preamble_tag" | "package_tag" => {
            let tag = doc.node_text(node.named_child(0)?).to_lowercase();
            let value = node.child_by_field_name("value")?;
            if tag.starts_with("source") || tag.starts_with("patch") {
                Some((value, true))
            } else if matches!(tag.trim_end_matches(':'), "url" | "bugurl" | "vcs") {
                Some((value, false))
            } else {
                None
            }
        }
        "url" | "file" if matches!(node.parent()?.kind(), "sourcelist" | "patchlist") => {
            Some((node, true))
        }
        _ => None,
    }
}

/// The spec's simple macros: one-line `%define`/`%global` values and the tag
/// values rpm makes macros of.
pub fn macro_values(doc: &Document) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for occurrence in macros::occurrences(doc).into_iter().filter(|o| o.definition) {
        let node = doc
            .tree
            .root_node()
            .descendant_for_byte_range(occurrence.bytes.start, occurrence.bytes.end);
        let Some(definition) = node.and_then(|n| n.parent()) else { continue };
        // The definition node ends before the brace of %{!?x: %global x 1}.
        let value = doc.text[occurrence.bytes.end..definition.end_byte()].trim();
        if !value.contains('\n') && !value.ends_with('\\') && !value.starts_with('(') {
            values.insert(occurrence.name, value.to_string());
        }
    }
    let mut cursor = doc.tree.root_node().walk();
    for node in doc.tree.root_node().named_children(&mut cursor) {
        if node.kind() != "preamble_tag" {
            continue;
        }
        let (Some(tag), Some(value)) = (node.named_child(0), node.child_by_field_name("value"))
        else {
            continue;
        };
        let tag = doc.node_text(tag).trim_end_matches(':').to_lowercase();
        if TAG_MACROS.contains(&tag.as_str()) {
            values.entry(tag).or_insert_with(|| doc.node_text(value).trim().to_string());
        }
    }
    values
}

/// `text` with its macros substituted, or `None` when one is unknown or too
/// complex: `%%`, `%name`, `%{name}`, and `%{?name}` (empty when undefined).
pub fn expand(text: &str, values: &HashMap<String, String>, depth: usize) -> Option<String> {
    if depth > 10 {
        return None;
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        if let Some(after) = rest.strip_prefix('%') {
            out.push('%');
            rest = after;
            continue;
        }
        let (body, after) = match rest.strip_prefix('{') {
            Some(braced) => {
                let end = braced.find('}')?;
                (&braced[..end], &braced[end + 1..])
            }
            None => {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        let (optional, name) = match body.strip_prefix('?') {
            Some(name) => (true, name),
            None => (false, body),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        match values.get(name) {
            Some(value) => out.push_str(&expand(value, values, depth + 1)?),
            None if optional => {}
            None => return None,
        }
        rest = after;
    }
    out.push_str(rest);
    Some(out)
}

pub fn doc_path(doc: &Document) -> Option<PathBuf> {
    url::Url::parse(doc.uri.as_str()).ok()?.to_file_path().ok()
}

pub fn doc_dir(doc: &Document) -> Option<PathBuf> {
    doc_path(doc)?.parent().map(Path::to_path_buf)
}

/// The source or patch file rpm uses for `value`, its basename, next to the
/// spec or in the SOURCES directory beside its SPECS directory.
fn local_file(dir: &Path, value: &str) -> Option<Uri> {
    file_uri(&local_path(dir, value)?.canonicalize().ok()?)
}

/// The local file of the source or patch `value`, by its basename, beside
/// the spec in `dir` or in the SOURCES directory beside it.
pub fn local_path(dir: &Path, value: &str) -> Option<PathBuf> {
    let name = basename(value)?;
    [dir.join(name), dir.join("../SOURCES").join(name)].into_iter().find(|p| p.is_file())
}

pub fn basename(value: &str) -> Option<&str> {
    value.rsplit('/').next().filter(|n| !n.is_empty())
}

pub fn file_uri(path: &Path) -> Option<Uri> {
    url::Url::from_file_path(path).ok()?.as_str().parse().ok()
}

/// A web URL; `git+https://` VCS URLs open as `https://`.
fn web(text: &str) -> Option<Uri> {
    let text = text.strip_prefix("git+").unwrap_or(text);
    ["https://", "http://", "ftp://"]
        .iter()
        .any(|scheme| text.starts_with(scheme))
        .then(|| text.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::parser;

    #[test]
    fn expand_substitutes_simple_macros_only() {
        let values: HashMap<String, String> =
            [("name", "foo"), ("version", "1.0"), ("srcname", "%{name}")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        assert_eq!(
            expand("%{srcname}-%version.tgz%{?dist}", &values, 0).as_deref(),
            Some("foo-1.0.tgz")
        );
        assert_eq!(expand("100%%", &values, 0).as_deref(), Some("100%"));
        assert_eq!(expand("%{unknown}/x", &values, 0), None);
        assert_eq!(expand("%(echo x)", &values, 0), None);
        assert_eq!(expand("%{lua: print(1)}", &values, 0), None);
    }

    #[test]
    fn links_point_to_local_files_and_urls() {
        let dir = std::env::temp_dir().join(format!("rpmls-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("SPECS")).unwrap();
        std::fs::create_dir_all(dir.join("SOURCES")).unwrap();
        std::fs::write(dir.join("SOURCES/foo-1.0.tar.gz"), "").unwrap();
        std::fs::write(dir.join("SPECS/fix.patch"), "").unwrap();
        let text = "\
%global srcname foo
%{!?ext: %global ext tar.gz}
Name: %{srcname}
Version: 1.0
URL: https://example.org/%{name}
VCS: git+https://example.org/foo.git
Source0: %{url}/%{name}-%{version}.%{ext}
Source1: https://example.org/missing.tgz
Source2: %(echo secret).tgz
Patch0: fix.patch
";
        let uri: Uri = file_uri(&dir.join("SPECS/foo.spec")).unwrap();
        let doc = Document::new(&mut parser(), uri, 1, text.into()).unwrap();
        let found: Vec<(u32, String)> = links(&doc)
            .into_iter()
            .map(|l| (l.range.start.line, l.target.unwrap().as_str().to_string()))
            .collect();
        let canonical = dir.canonicalize().unwrap();
        let local = |p: &str| file_uri(&canonical.join(p)).unwrap().as_str().to_string();
        assert_eq!(
            found,
            [
                (4, "https://example.org/foo".to_string()),
                (5, "https://example.org/foo.git".to_string()),
                (6, local("SOURCES/foo-1.0.tar.gz")),
                (7, "https://example.org/missing.tgz".to_string()),
                (9, local("SPECS/fix.patch")),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
