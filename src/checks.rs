//! rpmls's own checks, on what the linters do not see: the patch files in
//! the spec's directory, and whether the patches are applied.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Range};
use tree_sitter::Node;

use crate::document::Document;
use crate::links;

pub fn diagnostics(doc: &Document) -> Vec<Diagnostic> {
    patches(doc)
}

/// A `Source` or `Patch` tag: its number, its value expanded, if its macros
/// are simple, and its line.
struct Tag {
    patch: bool,
    number: Option<u32>,
    value: Option<String>,
    range: Range,
}

fn tags(doc: &Document) -> Vec<Tag> {
    let values = links::macro_values(doc);
    let mut tags = Vec::new();
    // Patch: and Source: without a number take the next one.
    let mut next: HashMap<bool, u32> = HashMap::new();
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit && node.kind() == "preamble_tag" {
            tags.extend(tag(doc, node, &values, &mut next));
        }
        if visit && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
        } else if cursor.goto_parent() {
            visit = false;
        } else {
            return tags;
        }
    }
}

fn tag(
    doc: &Document,
    node: Node,
    values: &HashMap<String, String>,
    next: &mut HashMap<bool, u32>,
) -> Option<Tag> {
    let name = doc.node_text(node.named_child(0)?).trim_end_matches(':').to_lowercase();
    let (patch, digits) = match (name.strip_prefix("patch"), name.strip_prefix("source")) {
        (Some(digits), _) => (true, digits),
        (_, Some(digits)) => (false, digits),
        _ => return None,
    };
    let number = match digits {
        "" => *next.get(&patch).unwrap_or(&0),
        digits => digits.parse().ok()?,
    };
    next.insert(patch, number + 1);
    let value = node.child_by_field_name("value")?;
    let value = links::expand(doc.node_text(value).trim(), values, 0);
    let line = node.start_position().row;
    let range = doc.range(node.start_byte()..node.start_byte() + doc.line(line).len());
    Some(Tag { patch, number: Some(number), value, range })
}

fn patches(doc: &Document) -> Vec<Diagnostic> {
    let Some(dir) = links::doc_dir(doc) else { return Vec::new() };
    let tags = tags(doc);
    let mut found = Vec::new();
    for tag in tags.iter().filter(|t| t.patch) {
        if let Some(value) = &tag.value
            && !value.contains("://")
            && links::local_path(&dir, value).is_none()
        {
            found.push(warning(tag.range, "patch-not-found", value));
        }
    }
    // On the last patch, else at the top.
    let at = tags.iter().rev().find(|t| t.patch).map_or(Range::default(), |t| t.range);
    found.extend(
        undeclared(doc, &dir, &tags).iter().map(|name| warning(at, "patch-not-declared", name)),
    );
    found.extend(unapplied(doc, &tags));
    found
}

/// The patch files of the spec's directory named neither by the spec, when it
/// is alone there and all its sources and patches are known, nor by the
/// other files there, like the patch lists a spec includes.
fn undeclared(doc: &Document, dir: &Path, tags: &[Tag]) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let names: Vec<String> =
        entries.filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned())).collect();
    let specs = names.iter().filter(|n| n.ends_with(".spec")).count();
    if specs > 1 || tags.iter().any(|t| t.value.is_none()) {
        return Vec::new();
    }
    let declared: BTreeSet<&str> =
        tags.iter().filter_map(|t| links::basename(t.value.as_deref()?)).collect();
    let is_patch = |n: &str| n.ends_with(".patch") || n.ends_with(".diff");
    // The small text files, but the spec, which the editor may be changing.
    let others: Vec<String> = names
        .iter()
        .filter(|n| !is_patch(n) && !n.ends_with(".spec"))
        .map(|n| dir.join(n))
        .filter(|p| p.metadata().is_ok_and(|m| m.is_file() && m.len() < 1 << 20))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    let mut found: Vec<String> = names
        .into_iter()
        .filter(|n| is_patch(n))
        .filter(|n| !declared.contains(n.as_str()) && !doc.text.contains(n.as_str()))
        .filter(|n| !others.iter().any(|text| text.contains(n.as_str())))
        .collect();
    found.sort();
    found
}

/// The patch files to declare, and the line of the last `Patch` tag, else of
/// the last `Source` tag, after which to declare them.
pub fn undeclared_patches(doc: &Document) -> (Vec<String>, Option<u32>) {
    let Some(dir) = links::doc_dir(doc) else { return (Vec::new(), None) };
    let tags = tags(doc);
    let last =
        |patch: bool| tags.iter().rev().find(|t| t.patch == patch).map(|t| t.range.start.line);
    (undeclared(doc, &dir, &tags), last(true).or_else(|| last(false)))
}

/// The numbered patches no `%patch` applies, unless `%autosetup` or
/// `%autopatch` applies them all.
fn unapplied(doc: &Document, tags: &[Tag]) -> Vec<Diagnostic> {
    let text = &doc.text;
    let auto = text.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("%autopatch")
            || l.starts_with("%{autopatch")
            || (l.starts_with("%autosetup") && !l.split_whitespace().any(|w| w == "-N"))
    });
    if auto || text.contains("%{patches}") || text.contains("%patches") {
        return Vec::new();
    }
    let mut applied = BTreeSet::new();
    let mut cursor = doc.tree.walk();
    let mut visit = true;
    loop {
        let node = cursor.node();
        if visit && node.kind() == "patch_macro" {
            applied.extend(applied_by(doc.node_text(node)));
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
    // Applied by hand, like `git am %{PATCH1}`.
    for prefix in ["%{PATCH", "%PATCH", "%{P:"] {
        for (i, _) in text.match_indices(prefix) {
            let digits: String =
                text[i + prefix.len()..].chars().take_while(char::is_ascii_digit).collect();
            applied.extend(digits.parse::<u32>().ok());
        }
    }
    tags.iter()
        .filter(|t| t.patch)
        .filter_map(|t| Some((t, t.number?)))
        .filter(|(_, n)| !applied.contains(n))
        .map(|(t, n)| warning(t.range, "patch-not-applied", &format!("Patch{n}")))
        .collect()
}

/// The patches a `%patch` line applies: `%patchN`, `-P N`, `-PN`, and its
/// positional numbers, or Patch0 alone, as rpm before 4.18.
fn applied_by(line: &str) -> Vec<u32> {
    let mut words = line.split_whitespace();
    let mut numbers = Vec::new();
    match words.next().and_then(|w| w.strip_prefix("%patch")) {
        Some("") => {}
        Some(n) => numbers.extend(n.parse::<u32>().ok()),
        None => return numbers,
    }
    let mut legacy_zero = numbers.is_empty();
    while let Some(word) = words.next() {
        let number = match word {
            "-P" => words.next(),
            // Options taking a value.
            "-p" | "-b" | "-z" | "-d" | "-F" | "-o" => {
                words.next();
                None
            }
            w if w.starts_with("-P") => Some(&w[2..]),
            w if !w.starts_with('-') => Some(w),
            _ => None,
        };
        if let Some(n) = number.and_then(|n| n.parse().ok()) {
            numbers.push(n);
            legacy_zero = false;
        }
    }
    if legacy_zero {
        numbers.push(0);
    }
    numbers
}

fn warning(range: Range, check: &str, details: &str) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        code: Some(NumberOrString::String(check.to_string())),
        source: Some("rpmls".to_string()),
        message: format!("{check} {details}"),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::parser;

    #[test]
    fn patch_lines_apply_their_numbers() {
        assert_eq!(applied_by("%patch0 -p1"), [0]);
        assert_eq!(applied_by("%patch12 -p1 -b .x"), [12]);
        assert_eq!(applied_by("%patch -P 3 -p 1"), [3]);
        assert_eq!(applied_by("%patch -P3 -P4"), [3, 4]);
        assert_eq!(applied_by("%patch 5 6 -p1"), [5, 6]);
        assert_eq!(applied_by("%patch -p1"), [0]);
    }

    #[test]
    fn patches_are_found_declared_and_applied() {
        let dir = std::env::temp_dir().join(format!("rpmls-checks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.patch", "b.patch", "stray.patch", "used-by-name.diff", "listed.patch"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::write(dir.join("foo.patches"), "Patch10: listed.patch\n").unwrap();
        let text = "\
Name: foo
Source0: foo.tgz
Patch0: a.patch
Patch1: b.patch
Patch2: missing.patch
Patch3: https://example.org/remote.patch
%prep
%setup -q
%patch -P0 -p1
cat used-by-name.diff | patch -p1
git am %{PATCH2}
";
        let uri = links::file_uri(&dir.join("foo.spec")).unwrap();
        std::fs::write(dir.join("foo.spec"), text).unwrap();
        let d = Document::new(&mut parser(), uri.clone(), 1, text.into()).unwrap();
        let found: Vec<(u32, String)> =
            diagnostics(&d).into_iter().map(|d| (d.range.start.line, d.message)).collect();
        assert_eq!(
            found,
            [
                (4, "patch-not-found missing.patch".to_string()),
                (5, "patch-not-declared stray.patch".to_string()),
                (3, "patch-not-applied Patch1".to_string()),
                (5, "patch-not-applied Patch3".to_string()),
            ]
        );
        // Declared at once, after the last patch, and completed after a Patch tag.
        let actions = crate::actions::actions(&d, &[], None);
        let lsp_types::CodeActionOrCommand::CodeAction(declare) = &actions[0] else { panic!() };
        assert_eq!(declare.title, "Declare stray.patch");
        let edit = &declare.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri][0];
        assert_eq!((edit.range.start.line, edit.new_text.as_str()), (6, "Patch4: stray.patch\n"));
        let typed =
            Document::new(&mut parser(), uri.clone(), 2, format!("{text}Patch4: st")).unwrap();
        let items =
            crate::completion::complete(&typed, lsp_types::Position::new(11, 10), None, Vec::new);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["listed.patch", "stray.patch"]);
        // All applied by %autosetup, and unknown sources tell nothing of the files.
        let text = "Name: foo\nSource0: %{url}/foo.tgz\nPatch0: a.patch\n%prep\n%autosetup -p1\n";
        let d = Document::new(&mut parser(), uri, 1, text.into()).unwrap();
        assert!(diagnostics(&d).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
