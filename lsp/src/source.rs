// Copyright 2026 Oxide Computer Company

//! Source files, spans, and position conversion.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Identifies a file loaded into a [`SourceDb`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range `[start, end)` within one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        Self { file, start, end }
    }

    /// The smallest span covering both `self` and `other`. Spans in
    /// different files cannot be joined; `self` is returned unchanged.
    pub fn to(self, other: Span) -> Span {
        if self.file != other.file {
            return self;
        }
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Whether `offset` lies within the span. The end offset counts as inside
    /// so that a cursor placed just after an identifier still selects it.
    pub fn contains(&self, file: FileId, offset: u32) -> bool {
        self.file == file && self.start <= offset && offset <= self.end
    }

    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Encoding used for the `character` component of LSP positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

/// Zero-based line and column, with the column measured in the units of a
/// [`PositionEncoding`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// Byte offsets of line starts, for converting between offsets and
/// line/column positions.
#[derive(Clone, Debug)]
pub struct LineIndex {
    starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i as u32 + 1);
            }
        }
        Self { starts }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset of the start of `line`, if it exists.
    pub fn line_start(&self, line: u32) -> Option<u32> {
        self.starts.get(line as usize).copied()
    }

    /// Zero-based line containing `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        (self.starts.partition_point(|&s| s <= offset) - 1) as u32
    }

    pub fn line_col(
        &self,
        text: &str,
        offset: u32,
        enc: PositionEncoding,
    ) -> LineCol {
        let offset = offset.min(text.len() as u32);
        let line = self.line_of(offset);
        let start = self.starts[line as usize] as usize;
        let mut end = offset as usize;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let prefix = &text[start..end];
        let col = match enc {
            PositionEncoding::Utf8 => prefix.len(),
            PositionEncoding::Utf16 => {
                prefix.chars().map(char::len_utf16).sum()
            }
            PositionEncoding::Utf32 => prefix.chars().count(),
        } as u32;
        LineCol { line, col }
    }

    /// Byte offset for a line/column position. Positions past the end of a
    /// line clamp to the end of that line, and positions past the last line
    /// clamp to the end of the text, as the LSP specification requires.
    pub fn offset(
        &self,
        text: &str,
        pos: LineCol,
        enc: PositionEncoding,
    ) -> u32 {
        let Some(start) = self.line_start(pos.line) else {
            return text.len() as u32;
        };
        let line_end = self
            .line_start(pos.line + 1)
            .map(|s| s as usize)
            .unwrap_or(text.len());
        let line = &text[start as usize..line_end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            if units >= pos.col {
                return start + i as u32;
            }
            units += match enc {
                PositionEncoding::Utf8 => c.len_utf8() as u32,
                PositionEncoding::Utf16 => c.len_utf16() as u32,
                PositionEncoding::Utf32 => 1,
            };
        }
        start + line.len() as u32
    }
}

#[derive(Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    pub text: Arc<str>,
    pub lines: LineIndex,
}

/// The set of files participating in one analysis.
///
/// Files are loaded from in-memory overlays (unsaved editor buffers) first and
/// from disk otherwise. Paths are stored as given; callers should pass
/// absolute, normalized paths so that one file maps to one [`FileId`].
#[derive(Debug, Default)]
pub struct SourceDb {
    files: Vec<SourceFile>,
    by_path: HashMap<PathBuf, FileId>,
    overlays: HashMap<PathBuf, Arc<str>>,
}

impl SourceDb {
    pub fn new(overlays: HashMap<PathBuf, Arc<str>>) -> Self {
        Self {
            overlays,
            ..Default::default()
        }
    }

    /// Add a file with the given contents, replacing nothing: if the path is
    /// already loaded its existing id is returned.
    pub fn add(&mut self, path: PathBuf, text: Arc<str>) -> FileId {
        if let Some(id) = self.by_path.get(&path) {
            return *id;
        }
        let id = FileId(self.files.len() as u32);
        let lines = LineIndex::new(&text);
        self.by_path.insert(path.clone(), id);
        self.files.push(SourceFile { path, text, lines });
        id
    }

    /// Load a file from the overlays or disk. Returns `None` if the file
    /// cannot be read.
    pub fn load(&mut self, path: &Path) -> Option<FileId> {
        if let Some(id) = self.by_path.get(path) {
            return Some(*id);
        }
        let text = match self.overlays.get(path) {
            Some(t) => t.clone(),
            None => {
                let mut s = std::fs::read_to_string(path).ok()?;
                // Normalize Windows line endings so offsets are consistent.
                if s.contains("\r\n") {
                    s = s.replace("\r\n", "\n");
                }
                Arc::from(s)
            }
        };
        Some(self.add(path.to_path_buf(), text))
    }

    /// Whether `path` names a readable file (overlay or disk), without
    /// loading it.
    pub fn exists(&self, path: &Path) -> bool {
        self.by_path.contains_key(path)
            || self.overlays.contains_key(path)
            || path.is_file()
    }

    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn text(&self, id: FileId) -> &str {
        &self.files[id.0 as usize].text
    }

    pub fn path(&self, id: FileId) -> &Path {
        &self.files[id.0 as usize].path
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(path).copied()
    }

    pub fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        (0..self.files.len() as u32).map(FileId)
    }

    /// Source text covered by a span.
    pub fn slice(&self, span: Span) -> &str {
        &self.text(span.file)[span.start as usize..span.end as usize]
    }

    pub fn line_col(
        &self,
        file: FileId,
        offset: u32,
        enc: PositionEncoding,
    ) -> LineCol {
        let f = self.file(file);
        f.lines.line_col(&f.text, offset, enc)
    }

    pub fn offset(
        &self,
        file: FileId,
        pos: LineCol,
        enc: PositionEncoding,
    ) -> u32 {
        let f = self.file(file);
        f.lines.offset(&f.text, pos, enc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_roundtrip_multibyte() {
        // 'é' is 2 bytes in UTF-8 and 1 unit in UTF-16; '𝄞' is 4 bytes in
        // UTF-8 and 2 units in UTF-16.
        let text = "ab\né𝄞x\n";
        let idx = LineIndex::new(text);
        let x = text.find('x').unwrap() as u32;
        assert_eq!(
            idx.line_col(text, x, PositionEncoding::Utf8),
            LineCol { line: 1, col: 6 }
        );
        assert_eq!(
            idx.line_col(text, x, PositionEncoding::Utf16),
            LineCol { line: 1, col: 3 }
        );
        assert_eq!(
            idx.line_col(text, x, PositionEncoding::Utf32),
            LineCol { line: 1, col: 2 }
        );
        for enc in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            let lc = idx.line_col(text, x, enc);
            assert_eq!(idx.offset(text, lc, enc), x);
        }
    }

    #[test]
    fn offset_clamps() {
        let text = "abc\nde";
        let idx = LineIndex::new(text);
        let enc = PositionEncoding::Utf16;
        assert_eq!(idx.offset(text, LineCol { line: 0, col: 99 }, enc), 3);
        assert_eq!(idx.offset(text, LineCol { line: 7, col: 0 }, enc), 6);
        assert_eq!(idx.offset(text, LineCol { line: 1, col: 1 }, enc), 5);
    }
}
