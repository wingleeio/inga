//! Byte-offset spans and line/column conversion.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Span {
        Span { start, end }
    }

    pub fn to(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn contains(self, offset: u32) -> bool {
        self.start <= offset && offset <= self.end
    }
}

/// Maps byte offsets to 0-based (line, column) pairs. Columns are in UTF-16
/// code units when produced via `line_col_utf16` (what LSP wants) and bytes
/// otherwise (what the CLI wants for slicing).
pub struct LineIndex {
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
    src_len: u32,
}

impl LineIndex {
    pub fn new(src: &str) -> LineIndex {
        let mut line_starts = vec![0u32];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        LineIndex { line_starts, src_len: src.len() as u32 }
    }

    /// 0-based line containing `offset`.
    pub fn line(&self, offset: u32) -> u32 {
        let offset = offset.min(self.src_len);
        match self.line_starts.binary_search(&offset) {
            Ok(line) => line as u32,
            Err(next) => (next - 1) as u32,
        }
    }

    pub fn line_start(&self, line: u32) -> u32 {
        self.line_starts.get(line as usize).copied().unwrap_or(self.src_len)
    }

    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// 0-based (line, byte column).
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let line = self.line(offset);
        (line, offset.min(self.src_len) - self.line_start(line))
    }

    /// 0-based (line, UTF-16 column) for LSP positions.
    pub fn line_col_utf16(&self, src: &str, offset: u32) -> (u32, u32) {
        let (line, byte_col) = self.line_col(offset);
        let start = self.line_start(line) as usize;
        let end = (start + byte_col as usize).min(src.len());
        let col16: usize = src[start..end].chars().map(|c| c.len_utf16()).sum();
        (line, col16 as u32)
    }

    /// Inverse of `line_col_utf16`.
    pub fn offset_utf16(&self, src: &str, line: u32, col16: u32) -> u32 {
        let start = self.line_start(line) as usize;
        let line_end = if (line + 1) < self.line_count() {
            self.line_start(line + 1) as usize
        } else {
            src.len()
        };
        let mut remaining = col16 as usize;
        for (i, c) in src[start..line_end].char_indices() {
            if remaining == 0 {
                return (start + i) as u32;
            }
            remaining = remaining.saturating_sub(c.len_utf16());
        }
        line_end as u32
    }
}
