//! PEP 701 aware scanning of f-string / template-string literal bodies.
//!
//! This module is shared by the lexer (which needs to find the *end* of a
//! string literal token) and by the string parser (which needs to scan a
//! nested string literal that appears inside a replacement-field expression).
//!
//! The scanner only locates structure: it copies raw source characters into
//! the output buffer, exactly like the pre-PEP 701 lexer did. Escape decoding
//! and expression parsing still happen later, but the scan respects replacement
//! fields, format specs, comments, escaped/newline rules and quote reuse so
//! that the literal boundaries are correct.
//!
//! Upstream `rustpython-parser` 0.4.0 scanned f-strings with a plain
//! "first matching quote wins" loop. That rejects PEP 701 programs such as
//! `f"{x["key"]}"` or `f"{f"{x}"}"`, so a real scanner is required.

use crate::lexer::{LexicalError, LexicalErrorType};
use crate::string::FStringErrorType;
use crate::text_size::TextSize;
use crate::token::StringKind;

/// Maximum nesting depth for *string literals inside replacement fields*.
/// This mirrors CPython's f-string nesting guard and prevents unbounded
/// recursion from corrupting the Rust stack on adversarial input.
pub(crate) const MAX_NESTED_STRINGS: u32 = 150;

/// Character source used by the shared scanner.
pub(crate) trait CharCursor {
    /// Consume and return the next character.
    fn advance(&mut self) -> Option<char>;
    /// Return the character `n` positions ahead without consuming it.
    fn peek_at(&self, n: usize) -> Option<char>;
    /// Current absolute byte offset in the source.
    fn position(&self) -> TextSize;
}

/// Which error vocabulary should be used for lexical failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanErrorKind {
    /// Errors reported by the lexer itself.
    Lexer,
    /// Errors raised while re-parsing an f-string's raw contents.
    FString,
}

#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    /// Number of `()[]{}` delimiters opened inside the replacement field's
    /// expression part. Used to tell a dict/set display from a field brace.
    bracket_depth: u32,
    /// True once a top-level `:` switched this replacement field into its
    /// format spec. Quotes are literal text in a format spec.
    in_spec: bool,
}

fn eof_error(
    kind: ScanErrorKind,
    triple: bool,
    unclosed_field: bool,
    pos: TextSize,
) -> LexicalError {
    match kind {
        ScanErrorKind::Lexer => LexicalError {
            error: if unclosed_field || !triple {
                LexicalErrorType::StringError
            } else {
                LexicalErrorType::Eof
            },
            location: pos,
        },
        ScanErrorKind::FString => LexicalError {
            error: LexicalErrorType::FStringError(if unclosed_field {
                FStringErrorType::UnclosedLbrace
            } else {
                FStringErrorType::UnterminatedString
            }),
            location: pos,
        },
    }
}

fn newline_error(kind: ScanErrorKind, pos: TextSize) -> LexicalError {
    match kind {
        ScanErrorKind::Lexer => LexicalError {
            error: LexicalErrorType::OtherError("EOL while scanning string literal".to_owned()),
            location: pos,
        },
        ScanErrorKind::FString => LexicalError {
            error: LexicalErrorType::FStringError(FStringErrorType::UnterminatedString),
            location: pos,
        },
    }
}

fn depth_error(kind: ScanErrorKind, pos: TextSize) -> LexicalError {
    match kind {
        ScanErrorKind::Lexer => LexicalError {
            error: LexicalErrorType::OtherError(
                "f-string expressions nested too deeply".to_owned(),
            ),
            location: pos,
        },
        ScanErrorKind::FString => LexicalError {
            error: LexicalErrorType::FStringError(FStringErrorType::ExpressionNestedTooDeeply),
            location: pos,
        },
    }
}

/// Return the string prefix (`f`, `rf`, `t`, ...) that immediately precedes
/// the quote stored as the last character of `out`, if any.
///
/// `out` ends with the opening quote of the nested literal; the quote itself is
/// not part of the prefix.
fn prefix_before_quote(out: &str) -> Option<StringKind> {
    let quote_len = out.chars().next_back()?.len_utf8();
    let before = &out[..out.len() - quote_len];
    let mut chars = before.chars().rev();
    let c1 = chars.next()?;
    if let Some(c2) = chars.next() {
        if let Ok(kind) = StringKind::try_from([c2, c1]) {
            return Some(kind);
        }
    }
    StringKind::try_from(c1).ok()
}

fn is_interpolating(out: &str) -> bool {
    prefix_before_quote(out)
        .map(|kind| kind.is_any_fstring() || kind.is_any_tstring())
        .unwrap_or(false)
}

/// Scan the body of an f-string / template-string literal.
///
/// * `quote`/`triple` describe the enclosing literal.
/// * `out` receives the raw source characters of the body. The opening quote
///   is *not* included and, when `include_closing_quote` is false, neither is
///   the closing quote (this matches `Tok::String::value`).
pub(crate) fn scan_literal_body<C: CharCursor>(
    cursor: &mut C,
    out: &mut String,
    quote: char,
    triple: bool,
    kind: ScanErrorKind,
) -> Result<(), LexicalError> {
    scan_body(cursor, out, quote, triple, true, kind, false, 0)
}

/// Scan a string literal which starts *after* its opening quote.
///
/// The opening quote must already be the last character of `out`; the closing
/// quote is appended to `out`. This is used while scanning a replacement-field
/// expression that contains a nested string literal.
pub(crate) fn scan_quoted_string<C: CharCursor>(
    cursor: &mut C,
    out: &mut String,
    quote: char,
    kind: ScanErrorKind,
    depth: u32,
) -> Result<(), LexicalError> {
    if depth > MAX_NESTED_STRINGS {
        return Err(depth_error(kind, cursor.position()));
    }
    let interpolating = is_interpolating(out);
    let triple = cursor.peek_at(0) == Some(quote) && cursor.peek_at(1) == Some(quote);
    if triple {
        out.push(cursor.advance().expect("peeked quote"));
        out.push(cursor.advance().expect("peeked quote"));
    }
    scan_body(cursor, out, quote, triple, interpolating, kind, true, depth)
}

#[allow(clippy::too_many_arguments)]
fn scan_body<C: CharCursor>(
    cursor: &mut C,
    out: &mut String,
    quote: char,
    triple: bool,
    interpolating: bool,
    kind: ScanErrorKind,
    include_closing_quote: bool,
    depth: u32,
) -> Result<(), LexicalError> {
    let mut frames: Vec<Frame> = Vec::new();
    loop {
        let ch = match cursor.advance() {
            Some(ch) => ch,
            None => {
                return Err(eof_error(
                    kind,
                    triple,
                    !frames.is_empty(),
                    cursor.position(),
                ))
            }
        };

        if ch == '\n' && !triple {
            return Err(newline_error(kind, cursor.position()));
        }

        if frames.is_empty() {
            if ch == quote {
                if triple {
                    if cursor.peek_at(0) == Some(quote) && cursor.peek_at(1) == Some(quote) {
                        if include_closing_quote {
                            out.push(ch);
                            out.push(cursor.advance().expect("peeked quote"));
                            out.push(cursor.advance().expect("peeked quote"));
                        } else {
                            cursor.advance();
                            cursor.advance();
                        }
                        return Ok(());
                    }
                    out.push(ch);
                } else {
                    if include_closing_quote {
                        out.push(ch);
                    }
                    return Ok(());
                }
                continue;
            }

            match ch {
                // A backslash escapes the next character for tokenization
                // purposes (even in raw strings) but must not swallow braces:
                // `\{` is still a literal backslash followed by a replacement
                // field in f-strings.
                '\\' => {
                    out.push(ch);
                    match cursor.peek_at(0) {
                        Some('{') | Some('}') => {}
                        Some(_) => out.push(cursor.advance().expect("peeked char")),
                        None => return Err(eof_error(kind, triple, false, cursor.position())),
                    }
                }
                '{' if interpolating => {
                    out.push(ch);
                    if cursor.peek_at(0) == Some('{') {
                        out.push(cursor.advance().expect("peeked char"));
                    } else {
                        frames.push(Frame::default());
                    }
                }
                '}' if interpolating => {
                    out.push(ch);
                    if cursor.peek_at(0) == Some('}') {
                        out.push(cursor.advance().expect("peeked char"));
                    }
                    // A lone `}` is left for the string parser to diagnose.
                }
                _ => out.push(ch),
            }
            continue;
        }

        // Inside a replacement field: copy raw text and track structure.
        out.push(ch);
        let in_spec = frames.last().expect("non-empty").in_spec;
        if in_spec {
            match ch {
                '{' => frames.push(Frame::default()),
                '}' => {
                    frames.pop();
                }
                _ => {}
            }
            continue;
        }

        match ch {
            '\'' | '"' => scan_quoted_string(cursor, out, ch, kind, depth + 1)?,
            '\\' => {
                if let Some(next) = cursor.advance() {
                    out.push(next);
                } else {
                    return Err(eof_error(kind, triple, false, cursor.position()));
                }
            }
            '#' => {
                // Comments are allowed inside replacement fields (PEP 701).
                // A single-quoted literal cannot contain the required newline.
                loop {
                    match cursor.advance() {
                        Some(next) => {
                            out.push(next);
                            if next == '\n' {
                                if !triple {
                                    return Err(newline_error(kind, cursor.position()));
                                }
                                break;
                            }
                        }
                        None => return Err(eof_error(kind, triple, false, cursor.position())),
                    }
                }
            }
            '{' | '[' | '(' => {
                frames.last_mut().expect("non-empty").bracket_depth += 1;
            }
            '}' | ']' | ')' => {
                let has_open_delimiter = frames
                    .last()
                    .map(|frame| frame.bracket_depth > 0)
                    .unwrap_or(false);
                if has_open_delimiter {
                    if let Some(frame) = frames.last_mut() {
                        frame.bracket_depth -= 1;
                    }
                } else if ch == '}' {
                    frames.pop();
                }
                // Unmatched `)`/`]` are left for the expression parser.
            }
            ':' => {
                let at_field_top = frames
                    .last()
                    .map(|frame| frame.bracket_depth == 0)
                    .unwrap_or(false);
                if at_field_top {
                    if let Some(frame) = frames.last_mut() {
                        frame.in_spec = true;
                    }
                }
            }
            _ => {}
        }
    }
}
