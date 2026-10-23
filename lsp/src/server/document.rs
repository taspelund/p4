// Copyright 2026 Oxide Computer Company

//! Open editor buffers.

use crate::source::{LineCol, LineIndex, PositionEncoding};
use lsp_types::{TextDocumentContentChangeEvent, Url};
use std::sync::Arc;

/// An open document, as last reported by the client.
#[derive(Debug)]
pub struct Document {
    /// The URI the client used, echoed back in notifications so the client
    /// can match them even when the path is not canonical (e.g. symlinks).
    pub uri: Url,
    pub version: i32,
    text: String,
    shared: Option<Arc<str>>,
}

impl Document {
    pub fn new(uri: Url, version: i32, text: String) -> Self {
        Self {
            uri,
            version,
            text,
            shared: None,
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text as a shared string for [`crate::source::SourceDb`]
    /// overlays. Cached until the next edit.
    pub fn shared(&mut self) -> Arc<str> {
        self.shared
            .get_or_insert_with(|| Arc::from(self.text.as_str()))
            .clone()
    }

    /// Apply `didChange` content changes in order. Each change's range is
    /// interpreted against the text produced by the previous change.
    pub fn apply_changes(
        &mut self,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
        enc: PositionEncoding,
    ) {
        for change in changes {
            apply_change(&mut self.text, &change, enc);
        }
        self.version = version;
        self.shared = None;
    }
}

/// Apply one content change. A change without a range replaces the whole
/// text.
pub fn apply_change(
    text: &mut String,
    change: &TextDocumentContentChangeEvent,
    enc: PositionEncoding,
) {
    let Some(range) = change.range else {
        text.clone_from(&change.text);
        return;
    };
    let index = LineIndex::new(text);
    let pos = |p: lsp_types::Position| {
        index.offset(
            text,
            LineCol {
                line: p.line,
                col: p.character,
            },
            enc,
        ) as usize
    };
    let start = pos(range.start);
    let end = pos(range.end).max(start);
    text.replace_range(start..end, &change.text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn change(
        (l1, c1): (u32, u32),
        (l2, c2): (u32, u32),
        text: &str,
    ) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(l1, c1),
                Position::new(l2, c2),
            )),
            range_length: None,
            text: text.into(),
        }
    }

    fn apply(
        text: &str,
        changes: &[TextDocumentContentChangeEvent],
        enc: PositionEncoding,
    ) -> String {
        let mut doc = Document::new(
            Url::parse("file:///t.p4").unwrap(),
            0,
            text.to_string(),
        );
        doc.apply_changes(1, changes.to_vec(), enc);
        doc.text().to_string()
    }

    #[test]
    fn surrogate_pairs() {
        // '𝄞' is 4 UTF-8 bytes, 2 UTF-16 units, and 1 UTF-32 unit.
        let text = "a𝄞bc\n";
        let c = [change((0, 3), (0, 4), "X")];
        assert_eq!(apply(text, &c, PositionEncoding::Utf16), "a𝄞Xc\n");
        assert_eq!(apply(text, &c, PositionEncoding::Utf32), "a𝄞bX\n");
        let c = [change((0, 5), (0, 6), "X")];
        assert_eq!(apply(text, &c, PositionEncoding::Utf8), "a𝄞Xc\n");
    }

    #[test]
    fn multibyte_utf8() {
        // 'é' is 2 UTF-8 bytes but one UTF-16 unit.
        let text = "x\né = 1;\n";
        let c = [change((1, 2), (1, 2), "+")];
        assert_eq!(apply(text, &c, PositionEncoding::Utf8), "x\né+ = 1;\n");
        assert_eq!(apply(text, &c, PositionEncoding::Utf16), "x\né += 1;\n");
    }

    #[test]
    fn sequential_changes() {
        // The second change is relative to the result of the first.
        let text = "abc\ndef\n";
        let c = [
            change((0, 0), (0, 0), "line\n"),
            change((2, 1), (2, 2), "E"),
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "whole".into(),
            },
            change((0, 5), (0, 5), "!"),
        ];
        assert_eq!(apply(text, &c, PositionEncoding::Utf16), "whole!");
        let c = [
            change((0, 0), (0, 0), "line\n"),
            change((2, 1), (2, 2), "E"),
        ];
        assert_eq!(
            apply(text, &c, PositionEncoding::Utf16),
            "line\nabc\ndEf\n"
        );
    }

    #[test]
    fn deletion_across_lines_and_clamping() {
        let text = "ab\ncd\nef";
        let c = [change((0, 1), (2, 1), "")];
        assert_eq!(apply(text, &c, PositionEncoding::Utf8), "af");
        // Past-the-end positions clamp to the end of the line / text.
        let c = [change((0, 99), (9, 0), "!")];
        assert_eq!(apply(text, &c, PositionEncoding::Utf8), "ab!");
    }
}
