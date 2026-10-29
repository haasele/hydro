//! Header ingestion: run clang's JSON AST dump and extract the declarations
//! the generator is allowed to see.
//!
//! The generator never talks to libclang; it invokes `clang` as a
//! subprocess, parses stdout with [`crate::json`] and keeps only function and
//! enum declarations that live in the requested header. Declarations from
//! included headers are ignored as callable surface (their typedefs remain
//! available for type resolution).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::json::{self, Json};

/// A spelled C type plus clang's desugared form, when it emitted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawType {
    pub spelling: String,
    pub desugared: Option<String>,
}

impl RawType {
    fn from_json(value: &Json) -> Option<RawType> {
        let spelling = value.get("qualType")?.as_str()?.to_owned();
        let desugared = value
            .get("desugaredQualType")
            .and_then(Json::as_str)
            .map(str::to_owned);
        Some(RawType {
            spelling,
            desugared,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RawParam {
    pub name: Option<String>,
    pub typ: RawType,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct RawFunction {
    pub name: String,
    pub line: u32,
    pub ret: RawType,
    pub params: Vec<RawParam>,
    pub variadic: bool,
    pub storage_static: bool,
    pub implicit: bool,
}

#[derive(Debug, Clone)]
pub struct RawTypedef {
    pub name: String,
    pub typ: RawType,
    pub implicit: bool,
}

#[derive(Debug, Clone)]
pub struct RawEnum {
    /// Tag name (`enum Color` → `Color`), if named.
    pub tag: Option<String>,
    pub variants: Vec<(String, i64)>,
}

/// Declarations collected from one AST dump.
#[derive(Debug, Default)]
pub struct RawHeader {
    /// Functions declared in the requested header, in source order.
    pub functions: Vec<RawFunction>,
    /// Functions in the requested header that are not exported by a shared
    /// library (implicit or `static`), with the reason.
    pub excluded: Vec<(String, u32, String)>,
    /// Functions seen outside the requested header (count only; they are
    /// headers included by it, not binding surface).
    pub foreign_functions: usize,
    /// Defensive parse errors, e.g. a ParmVarDecl without a type. Any entry
    /// stops generation rather than silently changing a signature.
    pub parse_errors: Vec<String>,
    pub typedefs: Vec<RawTypedef>,
    pub enums: Vec<RawEnum>,
}

/// How many bytes of clang stderr are echoed on failure.
const STDERR_LIMIT: usize = 8 * 1024;

/// Invoke clang and parse its JSON AST.
///
/// `includes` and `defines` are passed verbatim (`-I`/`-D`). The header is
/// canonicalized so location filtering compares real files.
pub fn ingest(
    header: &Path,
    includes: &[PathBuf],
    defines: &[String],
) -> Result<RawHeader, String> {
    let header = std::fs::canonicalize(header)
        .map_err(|error| format!("{}: cannot read header: {error}", header.display()))?;
    if !header.is_file() {
        return Err(format!("{}: not a regular file", header.display()));
    }

    let mut command = Command::new("clang");
    command
        .arg("-Xclang")
        .arg("-ast-dump=json")
        .arg("-fsyntax-only")
        .arg("-x")
        .arg("c-header");
    for dir in includes {
        command.arg("-I").arg(dir);
    }
    for define in defines {
        command.arg("-D").arg(define);
    }
    command.arg(&header);

    let output = command.output().map_err(|error| {
        format!(
            "cannot run `clang` (required for header ingestion): {error}; \
             install clang or put it on PATH"
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = truncate(&stderr, STDERR_LIMIT);
        return Err(format!(
            "clang failed for {} ({}):\n{}",
            header.display(),
            output.status,
            stderr.trim_end()
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("clang produced non-UTF-8 AST JSON for {}", header.display()))?;
    let ast = json::parse(&stdout).map_err(|error| {
        format!(
            "cannot parse clang AST JSON for {}: {error}",
            header.display()
        )
    })?;
    collect(&ast, &header)
}

fn truncate(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        text
    } else {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
}

/// Walk the translation unit and collect the declarations we care about.
fn collect(ast: &Json, header: &Path) -> Result<RawHeader, String> {
    let mut out = RawHeader::default();
    let root = ast
        .get("inner")
        .and_then(Json::as_array)
        .ok_or_else(|| "clang AST JSON has no TranslationUnitDecl body".to_owned())?;
    for node in root {
        walk(node, header, &mut out);
    }
    Ok(out)
}

fn walk(node: &Json, header: &Path, out: &mut RawHeader) {
    let kind = node.get("kind").and_then(Json::as_str).unwrap_or_default();
    match kind {
        "FunctionDecl" => {
            collect_function(node, header, out);
            // A function body cannot contain another FunctionDecl in C, but
            // nested declarations are harmless to skip here.
        }
        "TypedefDecl" => collect_typedef(node, header, out),
        "EnumDecl" => collect_enum(node, out),
        _ => {
            if let Some(inner) = node.get("inner").and_then(Json::as_array) {
                for child in inner {
                    walk(child, header, out);
                }
            }
        }
    }
}

fn collect_function(node: &Json, header: &Path, out: &mut RawHeader) {
    let Some(name) = node.get("name").and_then(Json::as_str) else {
        return;
    };
    let (file, line) = declaration_location(node);
    let implicit = node
        .get("isImplicit")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let in_header = match &file {
        Some(file) => same_file(file, header),
        None => {
            // clang omits `file` for main-file locations. A declaration from
            // an included header still carries `includedFrom`, which must not
            // be mistaken for the main file.
            !node
                .get("loc")
                .map(|loc| loc.get("includedFrom").is_some())
                .unwrap_or(false)
        }
    };
    if !in_header {
        out.foreign_functions += 1;
        return;
    }

    let storage_static = node
        .get("storageClass")
        .and_then(Json::as_str)
        .map(|storage| storage == "static")
        .unwrap_or(false);

    // Do not descend for the AST here: only direct parameters matter.
    let mut params = Vec::new();
    if let Some(inner) = node.get("inner").and_then(Json::as_array) {
        for child in inner {
            if child.get("kind").and_then(Json::as_str) != Some("ParmVarDecl") {
                continue;
            }
            let Some(typ) = child.get("type").and_then(RawType::from_json) else {
                out.parse_errors.push(format!(
                    "{name}:{}: parameter has no type in the clang AST",
                    line
                ));
                return;
            };
            let (_, param_line) = declaration_location(child);
            params.push(RawParam {
                name: child.get("name").and_then(Json::as_str).map(str::to_owned),
                typ,
                line: if param_line == 0 { line } else { param_line },
            });
        }
    }

    let Some(ret) = node
        .get("returnType")
        .and_then(RawType::from_json)
        .or_else(|| {
            node.get("type")
                .and_then(RawType::from_json)
                .map(|function_type| RawType {
                    spelling: return_part(&function_type.spelling).to_owned(),
                    desugared: None,
                })
        })
    else {
        out.parse_errors.push(format!(
            "{name}:{line}: declaration has no return type in the clang AST"
        ));
        return;
    };

    let variadic = node
        .get("variadic")
        .and_then(Json::as_bool)
        .unwrap_or(false)
        || ret.spelling.contains("...")
        || node
            .get("type")
            .and_then(|value| value.get("qualType"))
            .and_then(Json::as_str)
            .map(|qual| qual.contains("..."))
            .unwrap_or(false);

    if implicit {
        // Builtin/implicit declarations are not header surface; they are
        // ignored rather than reported as rejected symbols.
        return;
    }
    if storage_static {
        out.excluded.push((
            name.to_owned(),
            line,
            "static declaration; no external symbol to call".to_owned(),
        ));
        return;
    }

    out.functions.push(RawFunction {
        name: name.to_owned(),
        line,
        ret,
        params,
        variadic,
        storage_static,
        implicit,
    });
}

/// Return type from a clang function type spelling such as
/// `double (double, double)`.
///
/// A function returning a function pointer has a spelling like
/// `void (*(int))(int)`; the whole spelling is returned so the type table
/// rejects it instead of silently reading `void`.
fn return_part(qual_type: &str) -> &str {
    for (index, ch) in qual_type.char_indices() {
        if ch != '(' {
            continue;
        }
        let rest = qual_type[index..].trim_start();
        if rest.starts_with("(*") || rest.starts_with("(^") {
            return qual_type.trim();
        }
        return qual_type[..index].trim();
    }
    qual_type.trim()
}

fn collect_typedef(node: &Json, header: &Path, out: &mut RawHeader) {
    let Some(name) = node.get("name").and_then(Json::as_str) else {
        return;
    };
    let Some(typ) = node.get("type").and_then(RawType::from_json) else {
        return;
    };
    let (file, _line) = declaration_location(node);
    // Typedefs from included system headers are needed to resolve `size_t`,
    // `uint8_t` and similar names, so they are kept regardless of file.
    let _ = (header, file);
    out.typedefs.push(RawTypedef {
        name: name.to_owned(),
        typ,
        implicit: node
            .get("isImplicit")
            .and_then(Json::as_bool)
            .unwrap_or(false),
    });
}

fn collect_enum(node: &Json, out: &mut RawHeader) {
    let tag = node.get("name").and_then(Json::as_str).map(str::to_owned);
    let mut variants = Vec::new();
    let mut next = 0i64;
    if let Some(inner) = node.get("inner").and_then(Json::as_array) {
        for child in inner {
            if child.get("kind").and_then(Json::as_str) != Some("EnumConstantDecl") {
                continue;
            }
            let Some(name) = child.get("name").and_then(Json::as_str) else {
                continue;
            };
            let has_explicit = child.get("value").is_some()
                || child
                    .get("inner")
                    .and_then(Json::as_array)
                    .map(|inner| inner.iter().any(|node| node.get("value").is_some()))
                    .unwrap_or(false);
            let explicit = enum_value(child);
            if has_explicit && explicit.is_none() {
                out.parse_errors.push(format!(
                    "enum constant `{name}` has an explicit value that does not fit i64; \
                     enum constants must be tagged integers"
                ));
                continue;
            }
            let value = explicit.unwrap_or(next);
            next = value.saturating_add(1);
            variants.push((name.to_owned(), value));
        }
    }
    if !variants.is_empty() {
        out.enums.push(RawEnum { tag, variants });
    }
}

fn enum_value(node: &Json) -> Option<i64> {
    if let Some(value) = node.get("value") {
        if let Some(parsed) = parse_enum_literal(value) {
            return Some(parsed);
        }
    }
    for child in node.get("inner").and_then(Json::as_array)? {
        if let Some(value) = child.get("value") {
            if let Some(parsed) = parse_enum_literal(value) {
                return Some(parsed);
            }
        }
    }
    None
}

fn parse_enum_literal(value: &Json) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok();
    }
    text.parse::<i64>().ok()
}

/// Extract `(file, line)` from a clang `loc` object. Macro-expanded
/// locations are resolved to their expansion point, which is where the
/// declaration ultimately appears.
fn declaration_location(node: &Json) -> (Option<String>, u32) {
    let Some(loc) = node.get("loc") else {
        return (None, 0);
    };
    location(loc)
}

fn location(loc: &Json) -> (Option<String>, u32) {
    if loc.get("spellingLoc").is_some() || loc.get("expansionLoc").is_some() {
        // Prefer the expansion location: that is the source the declaration
        // occupies after preprocessing.
        if let Some(expansion) = loc.get("expansionLoc") {
            return location(expansion);
        }
        if let Some(spelling) = loc.get("spellingLoc") {
            return location(spelling);
        }
    }
    let line = loc
        .get("line")
        .and_then(Json::as_u64)
        .and_then(|line| u32::try_from(line).ok())
        .unwrap_or(0);
    let file = loc
        .get("file")
        .and_then(Json::as_str)
        .or_else(|| loc.get("presumedFile").and_then(Json::as_str))
        .map(str::to_owned);
    (file, line)
}

fn same_file(reported: &str, header: &Path) -> bool {
    let reported = Path::new(reported);
    if reported == header {
        return true;
    }
    match (
        std::fs::canonicalize(reported),
        std::fs::canonicalize(header),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Enum constants keyed by the tag spelling (`Color` and `enum Color`).
#[derive(Debug, Default)]
pub struct EnumTable {
    pub by_tag: BTreeMap<String, Vec<(String, i64)>>,
}

impl EnumTable {
    pub fn from_header(header: &RawHeader) -> EnumTable {
        let mut by_tag = BTreeMap::new();
        for value in &header.enums {
            if let Some(tag) = &value.tag {
                by_tag
                    .entry(tag.clone())
                    .or_insert_with(Vec::new)
                    .extend(value.variants.iter().cloned());
            }
        }
        EnumTable { by_tag }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_type_from_function_type() {
        assert_eq!(return_part("double (double, double)"), "double");
        assert_eq!(return_part("const char *(int)"), "const char *");
        assert_eq!(return_part("void (void)"), "void");
        assert_eq!(return_part("int (void, ...)"), "int");
    }

    #[test]
    fn enum_values_parse_decimal_and_hex() {
        assert_eq!(parse_enum_literal(&Json::String("42".into())), Some(42));
        assert_eq!(parse_enum_literal(&Json::String("0x10".into())), Some(16));
        assert_eq!(parse_enum_literal(&Json::String("-2".into())), Some(-2));
        assert_eq!(parse_enum_literal(&Json::Bool(true)), None);
    }

    #[test]
    fn collects_only_functions_in_the_requested_header() {
        let ast = crate::json::parse(
            r#"{
              "kind": "TranslationUnitDecl",
              "inner": [
                {"kind": "FunctionDecl", "name": "keep", "loc": {"line": 3},
                 "type": {"qualType": "double (double)"},
                 "inner": [{"kind": "ParmVarDecl", "name": "x", "loc": {"line": 3},
                            "type": {"qualType": "double"}}]},
                {"kind": "FunctionDecl", "name": "system_fn",
                 "loc": {"file": "/usr/include/stdio.h", "line": 10},
                 "type": {"qualType": "int (void)"}},
                {"kind": "FunctionDecl", "name": "implicit", "isImplicit": true,
                 "loc": {"line": 4}, "type": {"qualType": "int (void)"}},
                {"kind": "FunctionDecl", "name": "local_static", "storageClass": "static",
                 "loc": {"line": 5}, "type": {"qualType": "int (void)"}},
                {"kind": "TypedefDecl", "name": "size_alias",
                 "loc": {"file": "/usr/include/stddef.h", "line": 1},
                 "type": {"qualType": "unsigned long"}},
                {"kind": "EnumDecl", "name": "Color", "loc": {"line": 7},
                 "inner": [
                   {"kind": "EnumConstantDecl", "name": "RED", "loc": {"line": 7},
                    "inner": [{"kind": "ConstantExpr", "value": "0"}]},
                   {"kind": "EnumConstantDecl", "name": "BLUE", "loc": {"line": 7},
                    "inner": [{"kind": "ConstantExpr", "value": "2"}]}
                 ]}
              ]
            }"#,
        )
        .unwrap();
        let header = collect(&ast, Path::new("/tmp/lib.h")).unwrap();
        assert_eq!(header.functions.len(), 1);
        assert_eq!(header.functions[0].name, "keep");
        assert_eq!(header.functions[0].ret.spelling, "double");
        assert_eq!(header.functions[0].params.len(), 1);
        assert_eq!(header.foreign_functions, 1);
        assert_eq!(header.excluded.len(), 1);
        assert!(header
            .typedefs
            .iter()
            .any(|typedef| typedef.name == "size_alias"));
        assert_eq!(header.enums.len(), 1);
        assert_eq!(
            header.enums[0].variants,
            vec![("RED".to_owned(), 0), ("BLUE".to_owned(), 2)]
        );
    }

    #[test]
    fn function_pointer_return_is_not_read_as_void() {
        assert_eq!(return_part("void (*(int))(int)"), "void (*(int))(int)");
        assert_eq!(return_part("int (*)(double)"), "int (*)(double)");
    }
}
