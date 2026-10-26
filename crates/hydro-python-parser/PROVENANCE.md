# Provenance and license

This crate is a workspace-local fork of the published crate **`rustpython-parser`
0.4.0** (source: crates.io, `rustpython-parser-0.4.0`; upstream repository:
<https://github.com/RustPython/Parser>), authored by the RustPython Team and
licensed under the **MIT license**. All files under `src/` and `build.rs` are
copied verbatim from that release except for the files listed below.

The fork is private to this workspace (`publish = false`) and retains the
upstream MIT license. It is not a dependency of any published artifact.

## Why fork

`rustpython-parser` 0.4.0 implements the pre-PEP 701 f-string model: f-strings
are lexed as a single string token using a naive "first matching quote wins"
scan, replacement fields are re-parsed from a raw text buffer, and the lexer
does not know about `t`/`rt` template string prefixes. Supporting CPython 3.14
therefore requires changes at the lexer and string-parser level. A text-level
preprocessor would lose spans and could silently misparse valid programs, so
the parser itself is patched instead.

The generated LALR grammar (`src/python.lalrpop`, `src/python.rs`) is
**unchanged**, so the parse tables still match the SHA-3 stamp checked by
`build.rs`.

## Changes relative to upstream 0.4.0

- `src/scan.rs` (new): PEP 701-aware literal scanner shared by the lexer and
  the f-string parser. It tracks replacement-field nesting, brace-escaping,
  format specs, comments, escapes, and recursively nested string literals
  (including quote reuse), while copying raw source text so spans are
  preserved.
- `src/lexer.rs`: f-strings and template strings are lexed with `scan.rs`
  rather than the old naive quote scan. A `CharCursor` adapter exposes the
  lexer's character window to the shared scanner. No token shapes changed.
- `src/token.rs`: added `StringKind::TemplateString` (`t"/"T"`) and
  `StringKind::RawTemplateString` (`rt`/`tr`, case-insensitive), plus the
  matching `TryFrom`, `Display`, `prefix_len`, `is_raw`, and
  `is_any_tstring` implementations.
- `src/string.rs`:
  - replacement-field expressions are scanned with the shared PEP 701 rules
    (backslashes, comments, quote reuse, triple-quoted nested literals);
  - nested format-spec fields are parsed as expressions, so
    `f"{x:{"^10"}}"` is represented correctly;
  - `parse_strings` emits a tagged placeholder name for template-literal
    groups and rejects mixed template/non-template concatenation;
  - `parse_template_group` and `parse_template` build the template-string
    representation (see `src/template.rs`).
- `src/template.rs` (new): public CPython-3.14-shaped template representation
  (`TemplateString`, `TemplateConstant`, `Interpolation`), placeholder
  encoding/decoding, and the resolver that turns placeholder `Name` nodes into
  a `ParsedModule` (AST + `Vec<TemplateString>` side table).
- `src/parser.rs`: added `parse_module_with_templates`.
- `src/gen/parse.rs` and the `Parse` implementations in `src/parser.rs`: added
  the explicit anonymous lifetime `'_` to
  `SoftKeywordTransformer<Lexer<std::str::Chars<'_>>>` return types. This is
  the compiler-suggested spelling and silences rustc's
  `mismatched_lifetime_syntaxes` lint (the repository gate is
  `clippy -D warnings`); there is no behavioral change.
- `src/lib.rs`: exports the new modules and functions.
- `build.rs`: the upstream build script could optionally regenerate the LALR
  tables with the `lalrpop` build dependency. The fork always ships the
  pre-generated `src/python.rs` (checked against `src/python.lalrpop` by the
  SHA-3 stamp), so the optional `lalrpop` build dependency and that code path
  were removed. The generated grammar and tables themselves are byte-identical
  to upstream.
- Upstream's inline snapshot tests are retained verbatim but disabled
  (`#[cfg(any())]`): they require the `insta` dev-dependency, which is not
  vendored. `src/snapshots/` was removed for the same reason. New tests live in
  `tests/`.

## Semantics notes

- Template literals are represented in the RustPython AST by a placeholder
  `Expr::Name` whose identifier is `<hydro-tstring:START:END>` (byte offsets).
  Such identifiers cannot appear in Python source. The full structure is
  available through `ParsedModule::template_for_expr` /
  `template_for_range`; nested templates are resolved recursively and are
  present in the same table.
- Unsupported or invalid syntax produces a tagged error
  (`LexicalErrorType::TemplateError` or `LexicalErrorType::FStringError`); it
  is never silently misparsed.
