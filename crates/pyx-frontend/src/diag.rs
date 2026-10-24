//! User-facing diagnostics and source position mapping.
//!
//! A diagnostic is a first-class product: it carries a stable code, a source
//! span, an optional help hint and the file/line/column needed to render a
//! caret snippet. The CLI renders these; library users can use [`Display`] for
//! a compact `file:line:col: error: message` form.

use std::fmt;

/// Stable diagnostic code prefixes. Individual diagnostics should set a code
/// via [`Diagnostic::with_code`]; the default is [`codes::GENERIC`].
pub mod codes {
    /// Parse/syntax errors.
    pub const SYNTAX: &str = "PYX0001";
    /// Unsupported language construct in the current subset.
    pub const UNSUPPORTED: &str = "PYX0002";
    /// Generic semantic/type error.
    pub const GENERIC: &str = "PYX0100";
    /// Unknown name or did-you-mean.
    pub const NAME: &str = "PYX0101";
    /// Operand/type mismatch.
    pub const TYPE: &str = "PYX0102";
    /// Function call/arity errors.
    pub const CALL: &str = "PYX0103";
    /// Invalid return/break/continue placement.
    pub const CONTROL: &str = "PYX0104";
    /// Method/attribute errors.
    pub const ATTRIBUTE: &str = "PYX0201";
    /// Indexing/slicing/subscript errors.
    pub const INDEX: &str = "PYX0202";
    /// Annotation/definition validation.
    pub const DECL: &str = "PYX0301";
}

/// A user-facing diagnostic. Lines/columns are 1-based; the column is the
/// byte offset into the line (not a Unicode scalar index). `end_col` is
/// exclusive and covers the marked span (`col..end_col`); it defaults to
/// `col + 1` (one byte).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable machine-readable code, e.g. `PYX0102`.
    pub code: &'static str,
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// Exclusive end column of the marked span.
    pub end_col: u32,
    pub message: String,
    /// Optional actionable hint rendered as `help:`.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(file: &str, line: u32, col: u32, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            code: codes::GENERIC,
            file: file.to_string(),
            line,
            col,
            end_col: col.saturating_add(1),
            message: message.into(),
            help: None,
        }
    }

    /// Sets the stable diagnostic code (see [`codes`]).
    pub fn with_code(mut self, code: &'static str) -> Diagnostic {
        self.code = code;
        self
    }

    /// Sets the exclusive end column of the marked span. Values smaller than
    /// `col` are clamped to `col + 1` by [`Diagnostic::caret_len`].
    pub fn with_span(mut self, end_col: u32) -> Diagnostic {
        self.end_col = end_col;
        self
    }

    /// Adds an actionable hint displayed as `help:`.
    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }

    /// Number of columns the renderer should underline (at least one).
    pub fn caret_len(&self) -> u32 {
        self.end_col.saturating_sub(self.col).max(1)
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: error: {}",
            self.file, self.line, self.col, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}

/// Map a byte offset into the source to a 1-based `(line, column)` pair.
///
/// Columns are byte-based on purpose: they line up with the offsets supplied by
/// `rustpython-parser` and with the diagnostic convention documented in
/// `pyx_ir::Diagnostic`.
pub fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let bytes = source.as_bytes();
    let offset = offset.min(bytes.len());
    let mut line = 1u32;
    let mut line_start = 0usize;
    for (i, &b) in bytes[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    let col = (offset - line_start + 1) as u32;
    (line, col)
}

/// Span for a parser error, which only exposes the offending start offset.
///
/// The end column covers the token starting at the offset (an identifier or a
/// single character) so the renderer can underline something meaningful. It is
/// clamped to the start line and never narrower than `col + 1`.
pub(crate) fn syntax_span(source: &str, offset: usize) -> (u32, u32, u32) {
    let (line, col) = line_col(source, offset);
    let start = offset.min(source.len());
    let mut end = start;
    if let Some(rest) = source.get(start..) {
        if let Some(first) = rest.chars().next() {
            if first != '\n' && first != '\r' {
                let is_ident = |c: char| c.is_alphanumeric() || c == '_';
                if is_ident(first) {
                    for (index, c) in rest.char_indices() {
                        if is_ident(c) {
                            end = start + index + c.len_utf8();
                        } else {
                            break;
                        }
                    }
                } else {
                    end = start + first.len_utf8();
                }
            }
        }
    }
    let (end_line, end_col) = line_col(source, end);
    let end_col = if end_line > line { col } else { end_col };
    (line, col, end_col.max(col.saturating_add(1)))
}

/// Closest candidate to `name` with a Levenshtein distance of at most two.
pub(crate) fn closest<'a, I>(name: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    closest_within(name, candidates, 2)
}

/// Closest candidate to `name` within `max_distance` edit operations.
///
/// Ties keep the first candidate in iteration order, which makes the
/// suggestion deterministic for table-driven lookups.
pub(crate) fn closest_within<'a, I>(
    name: &str,
    candidates: I,
    max_distance: usize,
) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut best: Option<(&'a str, usize)> = None;
    for candidate in candidates {
        if candidate == name {
            continue;
        }
        let distance = levenshtein(name, candidate);
        let improves = match best {
            Some((_, current)) => distance < current,
            None => true,
        };
        if distance <= max_distance && improves {
            best = Some((candidate, distance));
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// Levenshtein edit distance between two strings, counted in characters.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_defaults_to_generic_with_one_column_span() {
        let diagnostic = Diagnostic::new("test.py", 2, 5, "boom");
        assert_eq!(diagnostic.code, codes::GENERIC);
        assert_eq!(diagnostic.end_col, 6);
        assert_eq!(diagnostic.caret_len(), 1);
        assert_eq!(diagnostic.help, None);
    }

    #[test]
    fn closest_prefers_the_smallest_edit_distance() {
        let candidates = ["value", "valley", "print"];
        assert_eq!(closest("valu", candidates), Some("value"));
        assert_eq!(closest("zzzzzz", candidates), None);
    }

    #[test]
    fn syntax_span_covers_the_offending_token() {
        let source = "def broken(:\n    pass\n";
        let offset = source.find(':').expect("colon");
        assert_eq!(syntax_span(source, offset), (1, 12, 13));
    }

    #[test]
    fn syntax_span_never_crosses_the_start_line() {
        let source = "abc\ndef";
        let (line, col, end_col) = syntax_span(source, 4);
        assert_eq!((line, col), (2, 1));
        assert_eq!(end_col, 4);
        assert!(end_col > col);
    }
}
