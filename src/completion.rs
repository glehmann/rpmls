//! Context-aware completion: tags, section keywords, `%files` directives and
//! macro names, each only where it can be written.

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemTag, CompletionTextEdit, Documentation,
    MarkupContent, MarkupKind, Position, TextEdit,
};

use crate::document::Document;
use crate::hover::{Manual, manual_key};
use crate::macros;

/// Preamble tags, and whether rpm deprecates them.
const TAGS: &[(&str, bool)] = &[
    ("Name", false),
    ("Version", false),
    ("Release", false),
    ("Epoch", false),
    ("Summary", false),
    ("License", false),
    ("SourceLicense", false),
    ("Group", false),
    ("URL", false),
    ("BugURL", false),
    ("VCS", false),
    ("BuildRequires", false),
    ("BuildConflicts", false),
    ("Requires", false),
    ("Requires(pre)", false),
    ("Requires(post)", false),
    ("Requires(preun)", false),
    ("Requires(postun)", false),
    ("Requires(pretrans)", false),
    ("Requires(posttrans)", false),
    ("Provides", false),
    ("Conflicts", false),
    ("Obsoletes", false),
    ("Recommends", false),
    ("Suggests", false),
    ("Supplements", false),
    ("Enhances", false),
    ("OrderWithRequires", false),
    ("BuildArch", false),
    ("ExclusiveArch", false),
    ("ExcludeArch", false),
    ("ExclusiveOS", false),
    ("ExcludeOS", false),
    ("AutoReq", false),
    ("AutoProv", false),
    ("AutoReqProv", false),
    ("BuildSystem", false),
    ("BuildOption", false),
    ("NoSource", false),
    ("NoPatch", false),
    ("Prefix", false),
    ("Packager", false),
    ("Vendor", false),
    ("Distribution", false),
    ("DistTag", false),
    ("ModularityLabel", false),
    ("BuildRoot", true),
    ("Prereq", true),
    ("BuildPrereq", true),
];

/// Keywords that start a line anywhere.
const KEYWORDS: &[&str] = &[
    "%package",
    "%description",
    "%prep",
    "%generate_buildrequires",
    "%conf",
    "%build",
    "%install",
    "%check",
    "%files",
    "%changelog",
    "%pre",
    "%post",
    "%preun",
    "%postun",
    "%pretrans",
    "%posttrans",
    "%preuntrans",
    "%postuntrans",
    "%verifyscript",
    "%triggerin",
    "%triggerun",
    "%triggerpostun",
    "%triggerprein",
    "%filetriggerin",
    "%filetriggerun",
    "%filetriggerpostun",
    "%transfiletriggerin",
    "%transfiletriggerun",
    "%transfiletriggerpostun",
    "%sourcelist",
    "%patchlist",
    "%if",
    "%ifarch",
    "%ifnarch",
    "%ifos",
    "%ifnos",
    "%elif",
    "%elifarch",
    "%elifos",
    "%else",
    "%endif",
    "%define",
    "%global",
    "%undefine",
];

/// Directives starting a `%files` line.
const FILES_DIRECTIVES: &[&str] = &[
    "%license",
    "%doc",
    "%docdir",
    "%config",
    "%config(noreplace)",
    "%config(missingok)",
    "%dir",
    "%ghost",
    "%attr(",
    "%defattr(",
    "%verify(",
    "%exclude",
    "%artifact",
    "%readme",
    "%missingok",
    "%caps(",
];

/// Where the word being typed starts, and how.
struct Typed {
    /// Where the item's text starts: at the `%` for `%name`, after the brace
    /// and operators for `%{name}`.
    start: usize,
    lead: Lead,
    /// Whether only blanks precede the word on its line.
    line_start: bool,
}

#[derive(PartialEq)]
enum Lead {
    None,
    Percent,
    Brace,
}

fn typed(doc: &Document, position: Position) -> Typed {
    let line = doc.line(position.line as usize);
    let line_offset = doc.offset(Position::new(position.line, 0));
    let cursor = doc.offset(position) - line_offset;
    let before = &line[..cursor];
    let word_start =
        before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
    let lead_text = &before[..word_start];
    let (lead, start) = if ["%{?", "%{!?", "%{"].iter().any(|l| lead_text.ends_with(l)) {
        (Lead::Brace, word_start)
    } else if lead_text.ends_with('%') && !lead_text.ends_with("%%") {
        (Lead::Percent, word_start - 1)
    } else {
        (Lead::None, word_start)
    };
    let line_start = before[..start.min(word_start)].trim().is_empty() && lead != Lead::Brace;
    Typed { start: line_offset + start, lead, line_start }
}

/// The section the line is in, or `None` in the preamble: the last section
/// header at or before it, found line by line like rpm does, so that the
/// half-typed lines of a buffer being edited cannot hide it.
fn section_at(doc: &Document, line: usize) -> Option<&'static str> {
    (0..=line).rev().find_map(|i| {
        let rest = doc.line(i).strip_prefix('%')?;
        let end =
            rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
        Some(match &rest[..end] {
            "package" => "package",
            "description" => "description",
            "files" => "files",
            "changelog" => "changelog",
            "sourcelist" | "patchlist" => "filelist",
            "prep"
            | "generate_buildrequires"
            | "conf"
            | "build"
            | "install"
            | "check"
            | "clean"
            | "verifyscript" => "scriptlet",
            k if k.starts_with("pre") || k.starts_with("post") || k.contains("trigger") => {
                "scriptlet"
            }
            _ => return None,
        })
    })
}

pub fn complete(
    doc: &Document,
    position: Position,
    manual: Option<&Manual>,
    system_macros: impl FnOnce() -> Vec<String>,
) -> Vec<CompletionItem> {
    if doc.line(position.line as usize).trim_start().starts_with('#') {
        return Vec::new();
    }
    let typed = typed(doc, position);
    let section = section_at(doc, position.line as usize);
    let range = doc.range(typed.start..doc.offset(position));
    let item = |label: String, kind, insert: String, deprecated: bool| {
        let docs = manual.and_then(|m| m.get(&manual_key(label.trim_end_matches(['(', ':', ' ']))));
        let documentation = docs.map(|text| {
            Documentation::MarkupContent(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text.to_string(),
            })
        });
        CompletionItem {
            filter_text: Some(label.clone()),
            label,
            kind: Some(kind),
            documentation,
            tags: deprecated.then(|| vec![CompletionItemTag::DEPRECATED]),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, insert))),
            ..Default::default()
        }
    };
    let mut items = Vec::new();
    match typed.lead {
        Lead::Percent | Lead::Brace => {
            if section == Some("changelog") {
                return items;
            }
            if typed.lead == Lead::Percent && typed.line_start {
                items.extend(KEYWORDS.iter().map(|k| {
                    item(k.to_string(), CompletionItemKind::KEYWORD, k.to_string(), false)
                }));
                if section == Some("files") {
                    items.extend(FILES_DIRECTIVES.iter().map(|d| {
                        item(d.to_string(), CompletionItemKind::KEYWORD, d.to_string(), false)
                    }));
                }
            }
            let mut names = local_macros(doc);
            names.extend(system_macros());
            names.sort();
            names.dedup();
            let percent = if typed.lead == Lead::Percent { "%" } else { "" };
            items.extend(names.into_iter().map(|name| {
                let text = format!("{percent}{name}");
                item(text.clone(), CompletionItemKind::VARIABLE, text, false)
            }));
        }
        Lead::None if typed.line_start && matches!(section, None | Some("package")) => {
            for (tag, deprecated) in TAGS {
                items.push(item(
                    format!("{tag}:"),
                    CompletionItemKind::FIELD,
                    format!("{tag}: "),
                    *deprecated,
                ));
            }
            if section.is_none() {
                for (tag, next) in next_numbers(doc) {
                    let label = format!("{tag}{next}:");
                    items.push(item(
                        label.clone(),
                        CompletionItemKind::FIELD,
                        format!("{label} "),
                        false,
                    ));
                }
            }
        }
        Lead::None => {}
    }
    items
}

fn local_macros(doc: &Document) -> Vec<String> {
    macros::occurrences(doc).into_iter().filter(|o| o.definition).map(|o| o.name).collect()
}

/// The next free numbers of the Source and Patch tags.
fn next_numbers(doc: &Document) -> [(&'static str, u32); 2] {
    let mut next = [("Source", 0), ("Patch", 0)];
    for line in doc.text.lines() {
        for (tag, n) in next.iter_mut() {
            let Some(rest) = line
                .get(..tag.len())
                .filter(|p| p.eq_ignore_ascii_case(tag))
                .map(|_| &line[tag.len()..])
            else {
                continue;
            };
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if rest[digits.len()..].starts_with(':') {
                *n = (*n).max(digits.parse::<u32>().map_or(1, |d| d + 1));
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;

    const SPEC: &str = "\
%global srcname foo
Name: %{srcname}
Source0: a.tgz
Source3: b.tgz
Patch0: p.patch
N

%description
Text %sr

%files
%li
/usr/bin/%{src

%changelog
* Mon Jan 01 2024 A <a@b> - 1-1
- %
";

    fn labels(line: u32, character: u32) -> Vec<String> {
        let d = doc(SPEC);
        complete(&d, Position::new(line, character), None, || vec!["_bindir".into()])
            .into_iter()
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn tags_complete_at_preamble_line_starts() {
        let found = labels(5, 1);
        assert!(found.contains(&"Name:".to_string()));
        assert!(found.contains(&"Source4:".to_string()));
        assert!(found.contains(&"Patch1:".to_string()));
        assert!(!found.iter().any(|l| l.starts_with('%')));
        // Not in a description's text.
        assert!(labels(8, 2).is_empty());
    }

    #[test]
    fn keywords_and_directives_complete_after_percent() {
        let found = labels(11, 3);
        assert!(found.contains(&"%license".to_string()));
        assert!(found.contains(&"%package".to_string()));
        assert!(found.contains(&"%srcname".to_string()));
        assert!(found.contains(&"%_bindir".to_string()));
        // Mid-line, only macros.
        let found = labels(8, 8);
        assert!(found.contains(&"%srcname".to_string()));
        assert!(!found.contains(&"%package".to_string()));
    }

    #[test]
    fn braced_macros_complete_without_percent() {
        let d = doc(SPEC);
        let items = complete(&d, Position::new(12, 14), None, Vec::new);
        let item = items.iter().find(|i| i.label == "srcname").unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else { panic!() };
        assert_eq!(edit.range.start, Position::new(12, 11));
        assert_eq!(edit.new_text, "srcname");
        // Nothing in the changelog.
        assert!(labels(16, 3).is_empty());
    }
}
