//! Hover: rpm's manual for tags, sections and directives, macro definitions,
//! and the explanation of the diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::Range;
use tree_sitter::Node;

use crate::document::Document;

/// Where rpm installs its spec manual.
const MANUAL_PATHS: &[&str] = &["/usr/share/doc/rpm/spec.md", "/usr/share/doc/rpm/manual/spec.md"];

/// rpm's spec file manual (`spec.md`), by heading.
pub struct Manual {
    /// Lowercase key → the heading and its text, as Markdown.
    entries: HashMap<String, String>,
}

impl Manual {
    /// The manual at `path`, or the installed one; `None` when there is none:
    /// container images are often installed without documentation.
    pub fn load(path: Option<&Path>) -> Option<Self> {
        let paths: Vec<PathBuf> = match path {
            Some(path) => vec![path.to_path_buf()],
            None => MANUAL_PATHS.iter().map(PathBuf::from).collect(),
        };
        paths.iter().find_map(|p| std::fs::read_to_string(p).ok()).map(|text| Self::parse(&text))
    }

    /// Index the text under each heading by the heading's first word (`Name`,
    /// `%prep`, `%license`), its whole text (`basic scriptlets`) and its
    /// `(or X)` alias. Adjacent headings share the text that follows them.
    pub fn parse(text: &str) -> Self {
        let mut entries = HashMap::new();
        let mut headings: Vec<&str> = Vec::new();
        let mut body: Vec<&str> = Vec::new();
        let mut flush = |headings: &mut Vec<&str>, body: &mut Vec<&str>| {
            let text = body.join("\n").trim().to_string();
            if !text.is_empty() {
                let title = headings.iter().map(|h| clean(h)).collect::<Vec<_>>().join(", ");
                for heading in headings.iter() {
                    for key in keys(heading) {
                        entries.entry(key).or_insert_with(|| format!("**{title}**\n\n{text}"));
                    }
                }
            }
            headings.clear();
            body.clear();
        };
        for line in text.lines() {
            if line.starts_with('#') {
                // Only headings with nothing between them share a text.
                if !body.is_empty() {
                    flush(&mut headings, &mut body);
                }
                headings.push(line);
            } else if !headings.is_empty() {
                body.push(line);
            }
        }
        flush(&mut headings, &mut body);
        Self { entries }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(&key.to_lowercase()).map(String::as_str)
    }
}

/// A heading without its `#` markers, backticks and annotations.
fn clean(heading: &str) -> String {
    let text = heading.trim_matches(|c: char| c == '#' || c.is_whitespace()).replace('`', "");
    let text = match text.find(" (since") {
        Some(i) => &text[..i],
        None => &text,
    };
    text.replace(" (OBSOLETE)", "").trim().to_string()
}

fn keys(heading: &str) -> Vec<String> {
    let text = clean(heading).to_lowercase();
    let first = text
        .split(|c: char| c.is_whitespace() || c == '(' || c == '[')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut keys = vec![first, text.clone()];
    if let Some(start) = text.find("(or ") {
        keys.push(text[start + 4..].trim_end_matches(')').trim().to_string());
    }
    keys
}

/// The manual heading documenting a keyword written in the spec.
fn manual_key(word: &str) -> String {
    let word = word.to_lowercase();
    match word.as_str() {
        "%pre" | "%post" | "%preun" | "%postun" | "%pretrans" | "%posttrans" | "%preuntrans"
        | "%postuntrans" | "%verifyscript" => "basic scriptlets".into(),
        "%triggerprein" | "%triggerin" | "%triggerun" | "%triggerpostun" => "triggers".into(),
        w if w.starts_with("%filetrigger") || w.starts_with("%transfiletrigger") => {
            "file triggers".into()
        }
        "%if" | "%ifarch" | "%ifnarch" | "%ifos" | "%ifnos" | "%elif" | "%elifarch" | "%elifos"
        | "%else" | "%endif" => "conditionals".into(),
        "%define" | "%global" | "%undefine" => "macros".into(),
        _ => word,
    }
}

/// The keyword under the cursor: a preamble tag, a section or conditional
/// keyword, or a `%files` directive, with its range.
pub fn keyword_at(doc: &Document, offset: usize) -> Option<(String, Range)> {
    let mut node = doc.tree.root_node().descendant_for_byte_range(offset, offset)?;
    // The parts of Requires(post): are the tokens of a dependency tag.
    if let Some(parent) = node.parent().filter(|p| is_tag(*p)) {
        node = parent;
    }
    let text = doc.node_text(node);
    let word = if is_tag(node) {
        // Source0: → Source, Requires(post): → Requires.
        let end = text.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(text.len());
        text[..end].to_string()
    } else if node.kind() == "builtin" && matches!(text, "setup" | "patch") {
        format!("%{text}")
    } else if text.starts_with('%')
        && (!node.is_named()
            || node.kind().starts_with("section_")
            || matches!(
                node.kind(),
                "file_qualifier" | "config_qualifier" | "attr" | "defattr" | "verify"
            ))
    {
        // %config(noreplace) → %config.
        let end = text[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(text.len(), |i| i + 1);
        text[..end].to_string()
    } else {
        return None;
    };
    Some((manual_key(&word), doc.range(node.byte_range())))
}

fn is_tag(node: Node) -> bool {
    matches!(node.kind(), "tag" | "dependency_tag")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;
    use lsp_types::Position;

    const MANUAL: &str = "\
# Spec file format
### Conditionals

Lines between %if and %endif.

#### Name

The name of the package.

#### Source

Sources.

#### NoSource
#### NoPatch

Marks sources and patches not to include.

#### Recommends (since rpm >= 4.13)
#### Suggests

#### OrderWithRequires

#### BuildArch (or BuildArchitectures)

The architecture.

### %prep

Prepares the sources.

### Basic scriptlets

Runs at install time.

#### `%license` (since rpm >= 4.11)

A license file.
";

    #[test]
    fn manual_indexes_headings_aliases_and_groups() {
        let manual = Manual::parse(MANUAL);
        assert!(manual.get("Name").unwrap().contains("The name of the package."));
        assert!(manual.get("nopatch").unwrap().starts_with("**NoSource, NoPatch**"));
        assert!(manual.get("suggests").is_none());
        assert!(manual.get("orderwithrequires").is_none());
        assert_eq!(manual.get("buildarchitectures"), manual.get("BuildArch"));
        assert!(manual.get("%license").unwrap().starts_with("**%license**"));
        assert!(manual.get("basic scriptlets").is_some());
        assert!(manual.get("conditionals").is_some());
    }

    #[test]
    fn keywords_map_to_manual_headings() {
        let d = doc(
            "Name: foo\nSource0: a.tgz\nRequires(post): x\n%if 1\n%endif\n\n%prep\n%setup -q\n\n%post\nx\n\n%files\n%config(noreplace) /etc/x\n%attr(0644,root,root) /a\n",
        );
        let key = |line, character| {
            keyword_at(&d, d.offset(Position::new(line, character))).map(|(k, _)| k)
        };
        assert_eq!(key(0, 2).as_deref(), Some("name"));
        assert_eq!(key(1, 3).as_deref(), Some("source"));
        assert_eq!(key(2, 3).as_deref(), Some("requires"));
        assert_eq!(key(3, 1).as_deref(), Some("conditionals"));
        assert_eq!(key(6, 2).as_deref(), Some("%prep"));
        assert_eq!(key(7, 2).as_deref(), Some("%setup"));
        assert_eq!(key(9, 2).as_deref(), Some("basic scriptlets"));
        assert_eq!(key(13, 3).as_deref(), Some("%config"));
        assert_eq!(key(14, 2).as_deref(), Some("%attr"));
        // Plain values are no keywords.
        assert_eq!(key(0, 7), None);
    }
}
