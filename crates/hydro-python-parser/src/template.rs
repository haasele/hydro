//! PEP 750 template-string representation and placeholder protocol.
//!
//! `rustpython-ast` 0.4.0 has no `TemplateStr` expression variant, and adding
//! one would mean patching a large amount of generated AST code (fold, visit,
//! unparse, located helpers, ...). Instead the parser keeps the existing
//! RustPython AST and represents a template-string literal as a reserved
//! [`Expr::Name`] placeholder:
//!
//! ```text
//! <hydro-tstring:START:END>
//! ```
//!
//! where `START`/`END` are the byte offsets of the whole literal group. Such a
//! name can never be produced by real Python source, so it cannot collide with
//! user code. The full structure of every template literal is returned in
//! [`ParsedModule::templates`]; resolve a placeholder with
//! [`ParsedModule::template_for_expr`] or [`ParsedModule::template_for_range`].
//!
//! This is an explicit, tagged protocol rather than a fake compatibility
//! shim: consumers that do not understand template strings cannot silently
//! treat them as some other expression with different semantics; they will see
//! a name that exists only in this protocol and can reject it with a clear
//! message.

use crate::ast;
use crate::lexer::{LexicalError, LexicalErrorType};
use crate::string::FStringErrorType;
use crate::text_size::{TextRange, TextSize};

use ast::fold::Fold;

/// Prefix of the reserved placeholder identifier.
pub const TEMPLATE_PLACEHOLDER_PREFIX: &str = "<hydro-tstring:";
/// Suffix of the reserved placeholder identifier.
pub const TEMPLATE_PLACEHOLDER_SUFFIX: &str = ">";

/// Maximum nesting of template literals inside interpolation expressions.
/// This mirrors the f-string guard used by the scanner.
pub const MAX_TEMPLATE_NESTING: u32 = 150;

/// Errors specific to template-string syntax that the f-string error type does
/// not cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateErrorType {
    /// A template string literal was implicitly concatenated with a
    /// non-template literal. PEP 750 forbids combining `Template` and `str`.
    MixedConcatenation,
    /// Only string literals may appear in a literal concatenation group.
    UnexpectedToken,
}

impl std::fmt::Display for TemplateErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MixedConcatenation => f.write_str(
                "cannot concatenate a template string with a non-template string literal",
            ),
            Self::UnexpectedToken => {
                f.write_str("unexpected token in template string literal group")
            }
        }
    }
}

/// A literal (non-interpolated) part of a template string.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateConstant {
    /// Decoded contents, exactly like the corresponding part of an f-string.
    pub value: String,
    /// Source range of the literal part (including prefix/quotes for the
    /// surrounding literal; individual parts cover their own text).
    pub range: TextRange,
}

/// One `{...}` interpolation inside a template string.
///
/// The shape mirrors CPython 3.14's `string.templatelib.Interpolation`:
/// `value`, `expression`, `conversion` and `format_spec`.
#[derive(Clone, Debug, PartialEq)]
pub struct Interpolation {
    /// Parsed expression AST. Its range is absolute in the original source.
    pub value: Box<ast::Expr>,
    /// Original source text of the expression (without `{`, `}`, conversion or
    /// format spec). Like CPython's `Interpolation.str`, surrounding whitespace
    /// is preserved exactly as written.
    pub expression: String,
    /// Conversion flag (`!s`, `!r`, `!a`), or `ConversionFlag::None`.
    pub conversion: ast::ConversionFlag,
    /// Format spec as an expression that evaluates to the spec string, i.e. a
    /// `JoinedStr` containing constants and interpolations, just like an
    /// f-string's `format_spec`. `None` when no `:` spec was written.
    pub format_spec: Option<Box<ast::Expr>>,
    /// Whether the interpolation used the debug `=` specifier.
    pub self_documenting: bool,
    /// Range of the whole interpolation, including the braces: `{...}`.
    pub range: TextRange,
    /// Range of just the expression source text.
    pub expression_range: TextRange,
}

/// A parsed template-string literal (or an implicit concatenation of template
/// literals).
///
/// `strings.len() == interpolations.len() + 1`, matching CPython's
/// `Template.strings` invariant.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateString {
    /// Literal parts, always one more than the number of interpolations.
    pub strings: Vec<TemplateConstant>,
    /// Interpolations in source order.
    pub interpolations: Vec<Interpolation>,
    /// Range of the complete literal group, including prefixes and quotes.
    pub range: TextRange,
    /// True for `rt"..."`/`tr"..."` template literals.
    pub raw: bool,
}

/// Result of parsing a module that may contain template strings.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedModule {
    /// The ordinary RustPython AST. Template-string literals appear as
    /// reserved [`ast::Expr::Name`] placeholders (see the module docs).
    pub module: ast::Mod,
    /// Every template literal in the module, including nested literals found
    /// inside interpolation expressions and format specs. Order is outermost
    /// first, depth-first.
    pub templates: Vec<TemplateString>,
}

impl ParsedModule {
    /// Look up a template literal by its source range.
    pub fn template_for_range(&self, range: TextRange) -> Option<&TemplateString> {
        self.templates
            .iter()
            .find(|template| template.range == range)
    }

    /// Index of the template literal represented by `expr`, if `expr` is a
    /// template placeholder.
    pub fn template_index_for_expr(&self, expr: &ast::Expr) -> Option<usize> {
        let ast::Expr::Name(name) = expr else {
            return None;
        };
        let range = template_placeholder_range(name.id.as_str())?;
        self.templates
            .iter()
            .position(|template| template.range == range)
    }

    /// Template literal represented by `expr`, if `expr` is a template
    /// placeholder.
    pub fn template_for_expr(&self, expr: &ast::Expr) -> Option<&TemplateString> {
        self.template_index_for_expr(expr)
            .map(|index| &self.templates[index])
    }
}

/// Build the reserved placeholder identifier for a template literal group.
pub fn template_placeholder_id(range: TextRange) -> String {
    format!(
        "{}{}:{}{}",
        TEMPLATE_PLACEHOLDER_PREFIX,
        range.start().to_u32(),
        range.end().to_u32(),
        TEMPLATE_PLACEHOLDER_SUFFIX
    )
}

/// Decode a reserved placeholder identifier back into its literal range.
/// Returns `None` for identifiers that are not part of the protocol.
pub fn template_placeholder_range(id: &str) -> Option<TextRange> {
    let inner = id
        .strip_prefix(TEMPLATE_PLACEHOLDER_PREFIX)?
        .strip_suffix(TEMPLATE_PLACEHOLDER_SUFFIX)?;
    let (start, end) = inner.split_once(':')?;
    let start = start.parse::<u32>().ok()?;
    let end = end.parse::<u32>().ok()?;
    (end >= start).then(|| TextRange::new(TextSize::from(start), TextSize::from(end)))
}

/// Returns true if `id` is a reserved template placeholder identifier.
pub fn is_template_placeholder(id: &str) -> bool {
    id.starts_with(TEMPLATE_PLACEHOLDER_PREFIX)
}

/// Recursively collect and parse every template literal referenced by the
/// placeholders in `module`.
pub(crate) fn resolve_module(source: &str, module: ast::Mod) -> Result<ParsedModule, LexicalError> {
    let mut collector = PlaceholderCollector::default();
    let module = match <PlaceholderCollector as Fold<TextRange>>::fold_mod(&mut collector, module) {
        Ok(module) => module,
        Err(never) => match never {},
    };

    let mut templates = Vec::new();
    for range in collector.ranges {
        templates.extend(resolve_template(source, range, 0)?);
    }
    Ok(ParsedModule { module, templates })
}

#[derive(Default)]
struct PlaceholderCollector {
    ranges: Vec<TextRange>,
}

impl Fold<TextRange> for PlaceholderCollector {
    type TargetU = TextRange;
    type Error = std::convert::Infallible;
    type UserContext = ();

    fn will_map_user(&mut self, _user: &TextRange) -> Self::UserContext {}

    fn map_user(
        &mut self,
        user: TextRange,
        _context: Self::UserContext,
    ) -> Result<Self::TargetU, Self::Error> {
        Ok(user)
    }

    fn fold_expr(&mut self, node: ast::Expr) -> Result<ast::Expr, Self::Error> {
        if let ast::Expr::Name(name) = &node {
            if let Some(range) = template_placeholder_range(name.id.as_str()) {
                self.ranges.push(range);
            }
        }
        ast::fold::fold_expr(self, node)
    }
}

fn dummy_expr() -> ast::Expr {
    ast::Expr::Constant(ast::ExprConstant {
        value: ast::Constant::None,
        kind: None,
        range: TextRange::empty(TextSize::from(0)),
    })
}

fn resolve_template(
    source: &str,
    range: TextRange,
    depth: u32,
) -> Result<Vec<TemplateString>, LexicalError> {
    if depth > MAX_TEMPLATE_NESTING {
        return Err(LexicalError {
            error: LexicalErrorType::FStringError(FStringErrorType::ExpressionNestedTooDeeply),
            location: range.start(),
        });
    }

    let mut template = crate::string::parse_template_group(source, range)?;
    let mut nested = Vec::new();
    for interpolation in &mut template.interpolations {
        let value = std::mem::replace(&mut *interpolation.value, dummy_expr());
        *interpolation.value = resolve_expr(source, value, depth + 1, &mut nested)?;
        if let Some(spec) = interpolation.format_spec.take() {
            interpolation.format_spec = Some(Box::new(resolve_expr(
                source,
                *spec,
                depth + 1,
                &mut nested,
            )?));
        }
    }

    let mut resolved = Vec::with_capacity(1 + nested.len());
    resolved.push(template);
    resolved.extend(nested);
    Ok(resolved)
}

fn resolve_expr(
    source: &str,
    expr: ast::Expr,
    depth: u32,
    resolved: &mut Vec<TemplateString>,
) -> Result<ast::Expr, LexicalError> {
    let mut collector = PlaceholderCollector::default();
    // Call the trait method (not the free function) so that a placeholder which
    // is the root expression is recorded as well as one nested inside it.
    let expr = match <PlaceholderCollector as Fold<TextRange>>::fold_expr(&mut collector, expr) {
        Ok(expr) => expr,
        Err(never) => match never {},
    };
    for range in collector.ranges {
        resolved.extend(resolve_template(source, range, depth)?);
    }
    Ok(expr)
}
