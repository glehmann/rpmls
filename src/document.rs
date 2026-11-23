//! An open spec file: its text, its syntax tree, and the conversions between
//! LSP positions (UTF-16 code units) and byte offsets.

use anyhow::{Result, anyhow};
use lsp_types::{Position, TextDocumentContentChangeEvent, Uri};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub tree: Tree,
    /// The byte offset where each line starts.
    line_starts: Vec<usize>,
}

impl Document {
    pub fn new(parser: &mut Parser, uri: Uri, version: i32, text: String) -> Result<Self> {
        let tree = parser.parse(&text, None).ok_or_else(|| anyhow!("parsing the spec"))?;
        let line_starts = line_starts(&text);
        Ok(Self { uri, version, text, tree, line_starts })
    }

    /// Apply the changes of a `didChange` notification, re-parsing the tree
    /// incrementally.
    pub fn apply(
        &mut self,
        parser: &mut Parser,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<()> {
        for change in changes {
            match change.range {
                Some(range) => {
                    let start = self.offset(range.start);
                    let end = self.offset(range.end).max(start);
                    let edit = InputEdit {
                        start_byte: start,
                        old_end_byte: end,
                        new_end_byte: start + change.text.len(),
                        start_position: self.point(start),
                        old_end_position: self.point(end),
                        new_end_position: point_after(self.point(start), &change.text),
                    };
                    self.text.replace_range(start..end, &change.text);
                    self.tree.edit(&edit);
                }
                None => {
                    self.text = change.text;
                    self.tree = parser.parse(&self.text, None).ok_or_else(|| anyhow!("parsing"))?;
                }
            }
            self.line_starts = line_starts(&self.text);
        }
        self.tree = parser
            .parse(&self.text, Some(&self.tree))
            .ok_or_else(|| anyhow!("parsing the spec"))?;
        self.version = version;
        Ok(())
    }

    /// The text of line `line`, without its line ending.
    pub fn line(&self, line: usize) -> &str {
        let start = self.line_starts.get(line).copied().unwrap_or(self.text.len());
        let end = self.line_starts.get(line + 1).copied().unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }

    /// The byte offset of an LSP position, clamped to the line and the text.
    pub fn offset(&self, position: Position) -> usize {
        let line = position.line as usize;
        let Some(&start) = self.line_starts.get(line) else {
            return self.text.len();
        };
        let mut units = 0;
        for (i, c) in self.line(line).char_indices() {
            if units >= position.character as usize {
                return start + i;
            }
            units += c.len_utf16();
        }
        start + self.line(line).len()
    }

    /// The LSP position of a byte offset.
    pub fn position(&self, offset: usize) -> Position {
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let start = self.line_starts[line];
        let character = self.text[start..offset.min(self.text.len())].encode_utf16().count();
        Position::new(line as u32, character as u32)
    }

    pub fn node_text(&self, node: Node) -> &str {
        &self.text[node.byte_range()]
    }

    fn point(&self, offset: usize) -> Point {
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        Point::new(line, offset - self.line_starts[line])
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

/// The point reached after inserting `text` at `start`.
fn point_after(start: Point, text: &str) -> Point {
    match text.rfind('\n') {
        Some(last) => Point::new(start.row + text.matches('\n').count(), text.len() - last - 1),
        None => Point::new(start.row, start.column + text.len()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lsp_types::Range;

    pub(crate) fn parser() -> Parser {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rpmspec::LANGUAGE.into()).unwrap();
        parser
    }

    pub(crate) fn doc(text: &str) -> Document {
        Document::new(&mut parser(), "file:///t.spec".parse().unwrap(), 1, text.into()).unwrap()
    }

    #[test]
    fn positions_count_utf16_units() {
        let d = doc("Name: é𝄞x\nVersion: 1\n");
        // é is one UTF-16 unit and two bytes, 𝄞 two units and four bytes.
        assert_eq!(d.offset(Position::new(0, 7)), 8);
        assert_eq!(d.offset(Position::new(0, 9)), 12);
        assert_eq!(d.position(12), Position::new(0, 9));
        assert_eq!(d.position(d.text.find("Version").unwrap()), Position::new(1, 0));
        assert_eq!(d.offset(Position::new(0, 99)), 13);
        assert_eq!(d.offset(Position::new(9, 0)), d.text.len());
    }

    #[test]
    fn incremental_changes_match_a_fresh_parse() {
        let mut p = parser();
        let mut d = doc("Name: foo\n%description\nx\n");
        let change = |line, from, to, text: &str| TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(line, from), Position::new(line, to))),
            range_length: None,
            text: text.into(),
        };
        d.apply(&mut p, 2, vec![change(0, 6, 9, "bar\nVersion: 1"), change(3, 0, 1, "y")]).unwrap();
        assert_eq!(d.text, "Name: bar\nVersion: 1\n%description\ny\n");
        assert_eq!(d.version, 2);
        let fresh = p.parse(&d.text, None).unwrap();
        assert_eq!(d.tree.root_node().to_sexp(), fresh.root_node().to_sexp());
    }
}
