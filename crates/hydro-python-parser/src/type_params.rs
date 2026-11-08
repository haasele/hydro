//! PEP 696 type-parameter defaults and PEP 758 unparenthesized exception
//! lists.
//!
//! The vendored `rustpython-ast` 0.4.0 and the pre-generated LALR tables in
//! `python.rs` predate both PEPs: `ast::TypeParam` has no `default` field and
//! `except_block` accepts a single expression. The tables cannot be
//! regenerated in this repository (no `lalrpop` generator is available), so
//! the two syntax extensions are applied as a token-stream rewrite in front of
//! the parser — the same tagged side-table approach the PEP 750 template
//! strings use:
//!
//! * A type-parameter default (`[T = int]`, `[T: int = int]`, `[*Ts = int]`,
//!   `[**P = int]`) is cut out of the stream; the parser then sees the PEP 695
//!   spelling it already understands, and the default expression is re-parsed
//!   from the original source into [`TypeParamDefaults`], keyed by the start
//!   offset of the parameter — exactly `ast::TypeParam::range().start()` for
//!   all three parameter kinds.
//! * `except A, B, C:` and `except* A, B:` get a synthetic `(`/`)` around the
//!   comma list; the grammar already parses that as a tuple, which is precisely
//!   the AST CPython builds for the parenthesised spelling.
//!
//! Both rewrites only remove tokens the grammar cannot accept, or add the
//! tokens the grammar would have produced for the equivalent explicit
//! spelling, so every diagnostic for genuinely unsupported syntax is
//! preserved: a default that does not parse is left in the stream and still
//! fails at the `=`, and `except A, B as e:` (which CPython rejects) is left
//! untouched.

use crate::ast::{self, Ranged};
use crate::lexer::LexResult;
use crate::parser::Parse;
use crate::text_size::{TextRange, TextSize};
use crate::token::Tok;

/// Type-parameter defaults of one module, keyed by the start offset of the
/// parameter that declares them.
///
/// `ast::TypeParam` carries no `default` field in `rustpython-ast` 0.4.0, so
/// the default expression lives here. Look it up with [`Self::for_param`] or
/// [`Self::get`]; both use the AST range, which survives the rewrite because
/// the removed tokens only ever follow the parameter header.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TypeParamDefaults {
    entries: Vec<(TextSize, ast::Expr)>,
}

impl TypeParamDefaults {
    /// Default declared by the type parameter that starts at `param_start`.
    pub fn get(&self, param_start: TextSize) -> Option<&ast::Expr> {
        self.entries
            .iter()
            .find(|(start, _)| *start == param_start)
            .map(|(_, default)| default)
    }

    /// Default declared by `param`, if any.
    pub fn for_param(&self, param: &ast::TypeParam) -> Option<&ast::Expr> {
        self.get(param.range().start())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (TextSize, &ast::Expr)> {
        self.entries.iter().map(|(start, expr)| (*start, expr))
    }

    fn insert(&mut self, start: TextSize, default: ast::Expr) {
        self.entries.push((start, default));
    }
}

/// Rewrite `tokens` into the subset of Python 3.14 syntax the vendored parser
/// understands.
///
/// Returns the rewritten stream plus the PEP 696 defaults that had to be cut
/// out of it. `source` must be the text the tokens were lexed from: defaults
/// are re-parsed from it so their spans stay meaningful.
pub fn rewrite_tokens(
    source: &str,
    source_path: &str,
    offset: TextSize,
    tokens: Vec<LexResult>,
) -> (Vec<LexResult>, TypeParamDefaults) {
    let mut defaults = TypeParamDefaults::default();
    let stripped = strip_type_param_defaults(
        source,
        source_path,
        offset,
        Slots::new(tokens),
        &mut defaults,
    );
    let rewritten = parenthesize_except_lists(stripped);
    (rewritten.into_tokens(), defaults)
}

/// A token vector whose elements can be inspected by index and moved out once.
///
/// `LexResult` is not `Clone`, so the rewrite pass takes each emitted token out
/// of its slot; lookahead only ever borrows.
struct Slots {
    tokens: Vec<Option<LexResult>>,
}

impl Slots {
    fn new(tokens: Vec<LexResult>) -> Self {
        Self {
            tokens: tokens.into_iter().map(Some).collect(),
        }
    }

    fn from_parts(parts: Vec<LexResult>) -> Self {
        Self::new(parts)
    }

    fn into_tokens(self) -> Vec<LexResult> {
        self.tokens.into_iter().flatten().collect()
    }

    fn len(&self) -> usize {
        self.tokens.len()
    }

    fn get(&self, index: usize) -> Option<&LexResult> {
        self.tokens.get(index).and_then(|slot| slot.as_ref())
    }

    fn tok(&self, index: usize) -> Option<&Tok> {
        match self.get(index) {
            Some(Ok((tok, _))) => Some(tok),
            _ => None,
        }
    }

    fn range(&self, index: usize) -> Option<TextRange> {
        match self.get(index) {
            Some(Ok((_, range))) => Some(*range),
            _ => None,
        }
    }

    fn start(&self, index: usize) -> Option<TextSize> {
        self.range(index).map(|range| range.start())
    }

    fn take(&mut self, index: usize) -> Option<LexResult> {
        self.tokens.get_mut(index).and_then(|slot| slot.take())
    }
}

fn is_open(tok: &Tok) -> bool {
    matches!(tok, Tok::Lpar | Tok::Lsqb | Tok::Lbrace)
}

fn is_close(tok: &Tok) -> bool {
    matches!(tok, Tok::Rpar | Tok::Rsqb | Tok::Rbrace)
}

/// True when `index` is the `[` that opens a PEP 695 type-parameter list,
/// i.e. it directly follows the name of a `def`, `class` or `type` statement.
fn opens_type_param_list(slots: &Slots, index: usize) -> bool {
    if !matches!(slots.tok(index), Some(Tok::Lsqb)) {
        return false;
    }
    let Some(name_index) = index.checked_sub(1) else {
        return false;
    };
    let Some(keyword_index) = index.checked_sub(2) else {
        return false;
    };
    let name = matches!(
        slots.tok(name_index),
        Some(Tok::Name { .. } | Tok::Type | Tok::Match | Tok::Case)
    );
    let keyword = matches!(
        slots.tok(keyword_index),
        Some(Tok::Def | Tok::Class | Tok::Type)
    );
    name && keyword
}

/// Cut every `= <default>` out of the type-parameter lists in `slots`.
fn strip_type_param_defaults(
    source: &str,
    source_path: &str,
    offset: TextSize,
    mut slots: Slots,
    defaults: &mut TypeParamDefaults,
) -> Slots {
    let mut out: Vec<LexResult> = Vec::with_capacity(slots.len());
    let mut index = 0;
    while index < slots.len() {
        if !opens_type_param_list(&slots, index) {
            if let Some(token) = slots.take(index) {
                out.push(token);
            }
            index += 1;
            continue;
        }

        // `index` is the opening bracket: emit it and walk the parameter list.
        if let Some(token) = slots.take(index) {
            out.push(token);
        }
        index += 1;

        let Some(close) = matching_close(&slots, index) else {
            // Unbalanced brackets: leave the remainder untouched so the parser
            // reports the real problem.
            while index < slots.len() {
                if let Some(token) = slots.take(index) {
                    out.push(token);
                }
                index += 1;
            }
            break;
        };

        let depth = token_depths(&slots, index, close);
        let mut segment_start = index;
        let mut cursor = index;
        while cursor <= close {
            let at_separator = cursor < close
                && depth[cursor] == 1
                && matches!(slots.tok(cursor), Some(Tok::Comma));
            if at_separator || cursor == close {
                emit_type_param(
                    source,
                    source_path,
                    offset,
                    &mut slots,
                    segment_start,
                    cursor,
                    &depth,
                    &mut out,
                    defaults,
                );
                if at_separator {
                    if let Some(token) = slots.take(cursor) {
                        out.push(token);
                    }
                }
                segment_start = cursor + 1;
            }
            cursor += 1;
        }

        // Emit the closing bracket (it was not consumed above: `cursor == close`
        // ends the loop without taking it).
        if let Some(token) = slots.take(close) {
            out.push(token);
        }
        index = close + 1;
    }
    Slots::from_parts(out)
}

/// Index of the bracket that closes the list opened just before `start`.
fn matching_close(slots: &Slots, start: usize) -> Option<usize> {
    let mut nesting = 1usize;
    let mut index = start;
    while index < slots.len() {
        match slots.tok(index) {
            Some(tok) if is_open(tok) => nesting += 1,
            Some(tok) if is_close(tok) => {
                nesting -= 1;
                if nesting == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Bracket depth of every token in `start..=close`, where the list's own
/// brackets are depth 0 and its contents depth 1.
fn token_depths(slots: &Slots, start: usize, close: usize) -> Vec<i32> {
    let mut depth = vec![0i32; close + 1];
    let mut level = 0i32;
    for index in start..=close {
        if matches!(slots.tok(index), Some(tok) if is_close(tok)) {
            level -= 1;
        }
        depth[index] = level;
        if matches!(slots.tok(index), Some(tok) if is_open(tok)) {
            level += 1;
        }
    }
    depth
}

/// Emit one type parameter `start..end`, recording its default if it has one.
#[allow(clippy::too_many_arguments)]
fn emit_type_param(
    source: &str,
    source_path: &str,
    offset: TextSize,
    slots: &mut Slots,
    start: usize,
    end: usize,
    depth: &[i32],
    out: &mut Vec<LexResult>,
    defaults: &mut TypeParamDefaults,
) {
    let param_start = slots.start(start);
    let mut equals = None;
    for index in start..end {
        if depth[index] == 1 && matches!(slots.tok(index), Some(Tok::Equal)) {
            equals = Some(index);
            break;
        }
    }

    // A default needs an expression after the `=`; anything else is a genuine
    // syntax error and is left for the parser to report.
    let default = equals.filter(|equals| *equals + 1 < end).and_then(|equals| {
        let range_start = slots.start(equals + 1)?;
        let range_end = slots.range(end - 1)?.end();
        let slice_start = usize::from(range_start.checked_sub(offset)?);
        let slice_end = usize::from(range_end.checked_sub(offset)?);
        let slice = source.get(slice_start..slice_end)?;
        ast::Expr::parse_starts_at(slice, source_path, range_start)
            .ok()
            .map(|expr| (equals, expr))
    });

    let Some((equals, expr)) = default else {
        for index in start..end {
            if let Some(token) = slots.take(index) {
                out.push(token);
            }
        }
        return;
    };

    // Emit the parameter header (`T`, `T: bound`, `*Ts`, `**P`) without the
    // `= default` tail.
    for index in start..equals {
        if let Some(token) = slots.take(index) {
            out.push(token);
        }
    }
    // The key is the parameter start, which is the start of its AST range for
    // all three parameter kinds.
    if let Some(param_start) = param_start {
        defaults.insert(param_start, expr);
    }
    for index in equals..end {
        slots.take(index);
    }
}

/// Wrap an unparenthesized `except` exception list in synthetic parentheses.
///
/// `except A, B:` is PEP 758 syntax; wrapping it yields the tuple AST CPython
/// builds. Clauses that already have one expression, that are bare
/// (`except:`), and the CPython-invalid `except A, B as e:` are left alone.
fn parenthesize_except_lists(mut slots: Slots) -> Slots {
    let mut out: Vec<LexResult> = Vec::with_capacity(slots.len());
    let mut index = 0;
    while index < slots.len() {
        if !matches!(slots.tok(index), Some(Tok::Except)) {
            if let Some(token) = slots.take(index) {
                out.push(token);
            }
            index += 1;
            continue;
        }

        // `except` and the optional `*` of an `except*` clause are emitted
        // first; the exception list starts after them.
        let mut list_start = index + 1;
        if let Some(token) = slots.take(index) {
            out.push(token);
        }
        if matches!(slots.tok(list_start), Some(Tok::Star)) {
            if let Some(token) = slots.take(list_start) {
                out.push(token);
            }
            list_start += 1;
        }

        if matches!(slots.tok(list_start), Some(Tok::Colon) | None) {
            index = list_start;
            continue;
        }

        let Some(clause) = scan_except_clause(&slots, list_start) else {
            index = list_start;
            continue;
        };
        if !clause.comma_list || clause.has_as {
            index = list_start;
            continue;
        }

        let Some(open_start) = slots.start(list_start) else {
            index = list_start;
            continue;
        };
        let Some(close_start) = slots.start(clause.colon) else {
            index = list_start;
            continue;
        };
        out.push(Ok((Tok::Lpar, TextRange::empty(open_start))));
        for inner in list_start..clause.colon {
            if let Some(token) = slots.take(inner) {
                out.push(token);
            }
        }
        out.push(Ok((Tok::Rpar, TextRange::empty(close_start))));
        index = clause.colon;
    }
    Slots::from_parts(out)
}

struct ExceptClause {
    /// Index of the `:` that terminates the clause header.
    colon: usize,
    /// The type list has a top-level comma followed by an expression.
    comma_list: bool,
    /// The clause binds with `as`, which forbids an unparenthesized list.
    has_as: bool,
}

fn scan_except_clause(slots: &Slots, start: usize) -> Option<ExceptClause> {
    let mut nesting = 0i32;
    let mut comma_list = false;
    let mut has_as = false;
    let mut index = start;
    while index < slots.len() {
        let tok = slots.tok(index)?;
        match tok {
            tok if is_open(tok) => nesting += 1,
            tok if is_close(tok) => nesting -= 1,
            Tok::Colon if nesting == 0 => {
                return Some(ExceptClause {
                    colon: index,
                    comma_list,
                    has_as,
                })
            }
            Tok::As if nesting == 0 => has_as = true,
            Tok::Comma if nesting == 0 => {
                // A comma only makes this an expression list when another
                // expression follows it; `except A,:` stays a syntax error.
                match slots.tok(index + 1) {
                    Some(Tok::Colon | Tok::As) | None => {}
                    Some(_) => comma_list = true,
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}
