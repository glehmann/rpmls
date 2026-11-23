//! Inlay hints: the values of the spec's simple macros, where they are used.

use lsp_types::{InlayHint, InlayHintLabel, Range};

use crate::document::Document;
use crate::links;
use crate::macros;

/// The longest value shown.
const LONGEST: usize = 40;

/// The values of the macros used in `range`, `%name`, `%{name}` and
/// `%{?name}`, after them, when the spec's simple macros resolve them, as
/// document links do, without running its code.
pub fn hints(doc: &Document, range: Range) -> Vec<InlayHint> {
    let values = links::macro_values(doc);
    let (start, end) = (doc.offset(range.start), doc.offset(range.end));
    let mut hints = Vec::new();
    for occurrence in macros::occurrences(doc) {
        if occurrence.definition || occurrence.bytes.start < start || occurrence.bytes.end > end {
            continue;
        }
        // After the brace of %{name}, but not in %{?name:…}, of another value.
        let after = &doc.text[occurrence.bytes.end..];
        let at = match after.chars().next() {
            Some('}') => occurrence.bytes.end + 1,
            Some(':') => continue,
            _ => occurrence.bytes.end,
        };
        let Some(value) = values.get(&occurrence.name).and_then(|v| links::expand(v, &values, 0))
        else {
            continue;
        };
        let shown: String = if value.chars().count() > LONGEST {
            value.chars().take(LONGEST).chain(['…']).collect()
        } else {
            value
        };
        hints.push(InlayHint {
            position: doc.position(at),
            label: InlayHintLabel::String(format!("= {shown}")),
            kind: None,
            text_edits: None,
            tooltip: None,
            padding_left: Some(true),
            padding_right: None,
            data: None,
        });
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::doc;
    use lsp_types::Position;

    #[test]
    fn simple_macros_show_their_values() {
        let d = doc("\
%global srcname foo
Name: %{srcname}
Version: 1.2
Source0: %{name}-%version.tgz %{?dist} %{?with:x}
Requires: %{unknown}
");
        let all = Range::new(Position::new(0, 0), Position::new(5, 0));
        let found: Vec<(u32, u32, String)> = hints(&d, all)
            .into_iter()
            .map(|h| match h.label {
                InlayHintLabel::String(label) => (h.position.line, h.position.character, label),
                InlayHintLabel::LabelParts(_) => panic!(),
            })
            .collect();
        let h = |line, character, label: &str| (line, character, label.to_string());
        assert_eq!(found, [h(1, 16, "= foo"), h(3, 16, "= foo"), h(3, 25, "= 1.2")]);
    }
}
