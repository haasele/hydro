//! Fixed C-to-HNA type table and function model construction.
//!
//! There is exactly one mapping table; heuristics and escape hatches are
//! deliberately absent. A declaration that does not fit the table stops
//! generation with the symbol, source line and reason.

use std::collections::{BTreeMap, BTreeSet};

use crate::clang::{RawFunction, RawHeader, RawParam, RawType};

/// The fixed projection table. `Str` and `Bytes` are pointer projections and
/// are only reachable for parameters and `const char *` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Void,
    /// Fixed-width integer; `ffi` is the Rust FFI type.
    Int {
        bits: u8,
        signed: bool,
        ffi: &'static str,
    },
    Bool,
    F32,
    F64,
    /// `enum X`, projected as a C `int`.
    Enum(String),
    /// NUL-terminated `const char *`.
    Str,
    /// `const char *`/`const uint8_t *` adjacent to a `size_t` length.
    Bytes,
}

impl Kind {
    /// Rust FFI type used in the `extern "C"` declaration.
    pub fn ffi_type(&self) -> &'static str {
        match self {
            Kind::Void => "()",
            Kind::Int { ffi, .. } => ffi,
            Kind::Bool => "bool",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Enum(_) => "c_int",
            Kind::Str => "*const core::ffi::c_char",
            Kind::Bytes => "core::ffi::c_void",
        }
    }

    /// Human description for the type report and diagnostics.
    pub fn description(&self) -> String {
        match self {
            Kind::Void => "None".to_owned(),
            Kind::Int { bits, signed, .. } => {
                format!(
                    "int ({}-bit {})",
                    bits,
                    if *signed { "signed" } else { "unsigned" }
                )
            }
            Kind::Bool => "bool".to_owned(),
            Kind::F32 | Kind::F64 => "float".to_owned(),
            Kind::Enum(tag) => format!("int (enum {tag})"),
            Kind::Str => "str (NUL-terminated UTF-8)".to_owned(),
            Kind::Bytes => "bytes (copied; adjacent size_t length)".to_owned(),
        }
    }

    pub fn int_range(&self) -> Option<(i64, i64)> {
        match self {
            Kind::Int { bits, signed, .. } => Some(if *signed {
                let bits = *bits as u32;
                if bits >= 64 {
                    (i64::MIN, i64::MAX)
                } else {
                    (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1)
                }
            } else {
                let bits = *bits as u32;
                let max = if bits >= 64 {
                    i64::MAX
                } else {
                    (1i64 << bits) - 1
                };
                (0, max)
            }),
            Kind::Enum(_) => Some((i32::MIN as i64, i32::MAX as i64)),
            _ => None,
        }
    }
}

/// One Python-visible parameter.
#[derive(Debug, Clone)]
pub struct Param {
    /// Sanitized Python keyword name.
    pub name: String,
    pub kind: Kind,
    /// Original C type spelling.
    pub c_type: String,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub c_name: String,
    pub line: u32,
    /// Position in source order; drives generated Rust identifiers.
    pub index: usize,
    pub ret: Kind,
    pub ret_c_type: String,
    pub params: Vec<Param>,
}

impl Function {
    pub fn min_args(&self) -> usize {
        self.params.len()
    }

    pub fn max_args(&self) -> usize {
        self.params.len()
    }

    pub fn params_string(&self) -> Option<String> {
        if self.params.is_empty() {
            None
        } else {
            Some(
                self.params
                    .iter()
                    .map(|param| param.name.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        }
    }
}

/// A type decision recorded for `type-report.md`.
#[derive(Debug, Clone)]
pub struct Decision {
    pub function: String,
    pub line: u32,
    pub item: String,
    pub c_type: String,
    pub mapping: String,
}

/// A successfully mapped module.
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub provider: String,
    pub header: String,
    pub functions: Vec<Function>,
    pub decisions: Vec<Decision>,
    pub excluded: Vec<(String, u32, String)>,
    pub foreign_functions: usize,
    /// Enum constants in deterministic name order.
    pub constants: Vec<(String, i64)>,
}

/// Unsupported declaration or type, with the point of failure.
#[derive(Debug, Clone)]
pub struct Unsupported {
    pub symbol: String,
    pub line: u32,
    pub detail: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: unsupported: {}",
            self.symbol, self.line, self.detail
        )
    }
}

struct Resolver<'a> {
    typedefs: BTreeMap<&'a str, &'a RawType>,
}

impl<'a> Resolver<'a> {
    fn new(header: &'a RawHeader) -> Resolver<'a> {
        let mut typedefs = BTreeMap::new();
        for typedef in &header.typedefs {
            if !typedef.implicit {
                typedefs
                    .entry(typedef.name.as_str())
                    .or_insert(&typedef.typ);
            }
        }
        Resolver { typedefs }
    }

    /// Expand a type to its desugared spelling when clang provided one.
    ///
    /// Leading qualifiers from the spelled type are preserved if the
    /// desugared form omits them; losing `const` here would turn a `str`
    /// input into a rejected mutable buffer.
    fn canonical(&self, typ: &RawType) -> String {
        let spelling = normalize(&typ.spelling);
        let Some(desugared) = &typ.desugared else {
            return spelling;
        };
        let desugared = normalize(desugared);
        if desugared.is_empty() {
            return spelling;
        }
        let (spelled_quals, _) = split_qualifiers(&spelling);
        let (desugared_quals, _) = split_qualifiers(&desugared);
        let missing: Vec<&str> = spelled_quals
            .split_whitespace()
            .filter(|qualifier| {
                !desugared_quals
                    .split_whitespace()
                    .any(|present| present == *qualifier)
            })
            .collect();
        if missing.is_empty() {
            desugared
        } else {
            format!("{} {}", missing.join(" "), desugared)
        }
    }

    /// Expand a bare typedef name (possibly with `const`/`volatile`) to its
    /// underlying type. Returns the input unchanged for non-typedefs.
    fn expand(&self, text: &str) -> String {
        let mut current = normalize(text);
        for _ in 0..16 {
            let (quals, base) = split_qualifiers(&current);
            let Some(target) = self.typedefs.get(base) else {
                return current;
            };
            let next = self.canonical(target);
            if next == base {
                return current;
            }
            current = if quals.is_empty() {
                next
            } else {
                format!("{quals} {next}")
            };
        }
        current
    }

    /// Classify one C type. Pointer projections are returned in the shape of
    /// the pointer, so the caller can apply the adjacency rules for length
    /// pairs.
    fn classify(&self, typ: &RawType) -> Result<Shape, String> {
        classify_type(&self.canonical(typ), self)
    }
}

/// Internal classification: `Value` is directly usable; the pointer shapes
/// still need the adjacency rule applied by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    Value(Kind),
    /// `const char *` without an adjacent length.
    StrPtr,
    /// `const uint8_t *`/`const unsigned char *`; requires a length.
    BytePtr,
    /// Mutable `char *`/`uint8_t *`; output buffers are not generated.
    MutablePtr,
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split leading type qualifiers from the rest.
fn split_qualifiers(text: &str) -> (&str, &str) {
    let text = text.trim();
    let bytes = text.as_bytes();
    let mut pos = 0;
    let mut end = 0;
    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos == bytes.len() {
            break;
        }
        let start = pos;
        while pos < bytes.len() && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let token = &text[start..pos];
        if matches!(token, "const" | "volatile" | "restrict" | "__restrict") {
            end = pos;
        } else {
            break;
        }
    }
    if end == 0 {
        ("", text)
    } else {
        (&text[..end], text[end..].trim_start())
    }
}

fn classify_type(canonical: &str, resolver: &Resolver<'_>) -> Result<Shape, String> {
    let canonical = normalize(canonical);
    if canonical.is_empty() {
        return Err("empty type spelling".to_owned());
    }
    if canonical.contains('[') {
        return Err("array and variable-length array types are not supported".to_owned());
    }
    if canonical.contains("(*") || canonical.contains("(^") {
        return Err("function pointer types are not supported".to_owned());
    }
    if canonical.starts_with("__builtin_va_list") || canonical == "va_list" {
        return Err("va_list has no HNA projection".to_owned());
    }
    if canonical.contains("jmp_buf") {
        return Err("jmp_buf has no HNA projection".to_owned());
    }

    if canonical.contains('*') {
        return classify_pointer(&canonical, resolver);
    }

    let expanded = resolver.expand(&canonical);
    let expanded = normalize(&expanded);
    if expanded.contains('*') {
        return classify_pointer(&expanded, resolver);
    }
    if expanded.contains('[') || expanded.contains("(*") {
        return Err(format!("typedef expands to unsupported type `{expanded}`"));
    }
    let (_, base) = split_qualifiers(&expanded);
    let base = base.trim();

    if base == "void" {
        return Ok(Shape::Value(Kind::Void));
    }
    if base.starts_with("long double") {
        return Err("long double has no HNA projection".to_owned());
    }
    if base.starts_with("union ") {
        return Err("union types are not supported".to_owned());
    }
    if base.starts_with("struct ") {
        return Err(
            "struct by value is not supported (layout-based POD projections are not generated in v1)"
                .to_owned(),
        );
    }
    if let Some(tag) = base.strip_prefix("enum ") {
        return Ok(Shape::Value(Kind::Enum(tag.trim().to_owned())));
    }

    if let Some(kind) = scalar_kind(base) {
        return Ok(Shape::Value(kind));
    }

    Err(format!(
        "type `{}` is not in the fixed projection table",
        base
    ))
}

fn scalar_kind(base: &str) -> Option<Kind> {
    let base = normalize(base);
    let base = base.as_str();
    let int = |bits: u8, signed: bool, ffi: &'static str| Kind::Int { bits, signed, ffi };
    match base {
        "bool" | "_Bool" => Some(Kind::Bool),
        "float" => Some(Kind::F32),
        "double" => Some(Kind::F64),
        "signed char" | "int8_t" | "__int8_t" => Some(int(8, true, "i8")),
        "unsigned char" | "uint8_t" | "__uint8_t" => Some(int(8, false, "u8")),
        "short" | "short int" | "signed short" | "signed short int" | "int16_t" => {
            Some(int(16, true, "i16"))
        }
        "unsigned short" | "unsigned short int" | "uint16_t" => Some(int(16, false, "u16")),
        "int" | "signed" | "signed int" | "int32_t" => Some(int(32, true, "i32")),
        "unsigned" | "unsigned int" | "uint32_t" => Some(int(32, false, "u32")),
        "long"
        | "long int"
        | "signed long"
        | "signed long int"
        | "long long"
        | "long long int"
        | "signed long long"
        | "signed long long int"
        | "int64_t"
        | "ptrdiff_t"
        | "intptr_t"
        | "intmax_t" => Some(int(64, true, "i64")),
        "unsigned long"
        | "unsigned long int"
        | "unsigned long long"
        | "unsigned long long int"
        | "uint64_t"
        | "size_t"
        | "uintptr_t"
        | "uintmax_t" => Some(int(64, false, "u64")),
        "char" => None,
        _ => None,
    }
}

/// Pointer projections.
///
/// `const char *` maps to `str`; `const char *`/`const uint8_t *` followed by
/// `size_t` maps to `bytes`. Every other pointer (including mutable output
/// buffers, scalar pointers, record handles and `void *`) is rejected with an
/// explicit diagnostic.
fn classify_pointer(canonical: &str, resolver: &Resolver<'_>) -> Result<Shape, String> {
    let Some(star) = canonical.find('*') else {
        return Err("malformed pointer type".to_owned());
    };
    let pointee = canonical[..star].trim();
    let after = canonical[star + 1..].trim();
    if after.contains('*') {
        return Err("multi-level pointers are not supported".to_owned());
    }
    let (quals, pointee_base) = split_qualifiers(pointee);
    let mut is_const = quals.split_whitespace().any(|qual| qual == "const");
    let expanded = resolver.expand(pointee_base);
    let expanded = normalize(&expanded);
    let (expanded_quals, expanded_base) = split_qualifiers(&expanded);
    is_const |= expanded_quals
        .split_whitespace()
        .any(|qual| qual == "const");
    let expanded_base = expanded_base.trim();

    if expanded_base == "char" {
        if !is_const {
            return Ok(Shape::MutablePtr);
        }
        return Ok(Shape::StrPtr);
    }
    if expanded_base == "unsigned char" || expanded_base == "uint8_t" {
        if !is_const {
            return Ok(Shape::MutablePtr);
        }
        return Ok(Shape::BytePtr);
    }
    if expanded_base == "void" {
        return Err("void* has no HNA projection".to_owned());
    }
    if expanded_base.starts_with("struct ") || expanded_base.starts_with("union ") {
        if expanded_base.starts_with("union ") {
            return Err("union pointers are not supported".to_owned());
        }
        return Err("opaque struct pointer handles are not generated in v1".to_owned());
    }
    Err(format!(
        "pointer to `{expanded_base}` is not supported (only const char*/const uint8_t* with a size_t length pair)"
    ))
}

/// `size_t` as written, before desugaring to an unsigned integer.
fn is_size_t(typ: &RawType) -> bool {
    let text = typ.spelling.trim();
    let (_, base) = split_qualifiers(text);
    base == "size_t"
}

/// Sanitize a C parameter name into a Python identifier usable as a keyword
/// name in the descriptor.
fn py_name(raw: Option<&str>, index: usize, used: &mut BTreeSet<String>) -> String {
    let raw = raw.unwrap_or("");
    if raw.is_empty() {
        let name = format!("arg{index}");
        used.insert(name.clone());
        return name;
    }
    let mut name = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            name.push(ch);
        } else if ch == '\0' {
            continue;
        } else {
            name.push('_');
        }
    }
    if name.is_empty() || name.starts_with(|ch: char| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }
    if is_python_keyword(&name) {
        name.insert(0, '_');
    }
    if !used.insert(name.clone()) {
        let mut suffix = 2;
        loop {
            let candidate = format!("{name}_{suffix}");
            if used.insert(candidate.clone()) {
                name = candidate;
                break;
            }
            suffix += 1;
        }
    }
    if name.is_empty() {
        name = format!("arg{index}");
        used.insert(name.clone());
    }
    name
}

fn is_python_keyword(name: &str) -> bool {
    matches!(
        name,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

/// Validate the module import name and return its components.
pub fn validate_module_name(name: &str) -> Result<Vec<&str>, String> {
    if name.is_empty() {
        return Err("module name is empty".to_owned());
    }
    let components: Vec<&str> = name.split('.').collect();
    for component in &components {
        if component.is_empty() {
            return Err(format!(
                "module name `{name}` has an empty component (leading, trailing or doubled dot)"
            ));
        }
        let mut chars = component.chars();
        let first = chars.next().expect("component is non-empty");
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(format!(
                "module name component `{component}` must start with a letter or underscore"
            ));
        }
        if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            return Err(format!(
                "module name component `{component}` contains a non-identifier character"
            ));
        }
    }
    Ok(components)
}

fn validate_c_string(what: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{what} contains a NUL byte"));
    }
    Ok(())
}

fn validate_export_name(name: &str) -> Result<(), String> {
    validate_c_string("function name", name)?;
    if name.is_empty() {
        return Err("function has an empty name".to_owned());
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(format!(
            "function name `{name}` is not a C identifier and cannot be an export name"
        ));
    }
    if name.starts_with(|ch: char| ch.is_ascii_digit()) {
        return Err(format!("function name `{name}` starts with a digit"));
    }
    Ok(())
}

/// Map every declaration in the header, rejecting the first unsupported one.
pub fn build_module(
    header: &RawHeader,
    module_name: &str,
    provider: &str,
    header_path: &str,
) -> Result<Module, Unsupported> {
    validate_module_name(module_name).map_err(|detail| Unsupported {
        symbol: module_name.to_owned(),
        line: 0,
        detail,
    })?;
    validate_c_string("provider", provider).map_err(|detail| Unsupported {
        symbol: module_name.to_owned(),
        line: 0,
        detail,
    })?;

    if let Some(error) = header.parse_errors.first() {
        return Err(Unsupported {
            symbol: "<header>".to_owned(),
            line: 0,
            detail: error.clone(),
        });
    }

    let resolver = Resolver::new(header);
    let mut functions = Vec::new();
    let mut decisions = Vec::new();
    let mut enums = BTreeSet::new();
    let mut seen_names = BTreeSet::new();

    for raw in &header.functions {
        let fail = |detail: String| Unsupported {
            symbol: raw.name.clone(),
            line: raw.line,
            detail,
        };
        validate_export_name(&raw.name).map_err(fail)?;
        if raw.variadic {
            return Err(fail(
                "variadic functions are not supported; wrap a fixed-arity subset explicitly"
                    .to_owned(),
            ));
        }
        if raw.implicit || raw.storage_static {
            // Already recorded as excluded; defensive only.
            continue;
        }
        if !seen_names.insert(raw.name.clone()) {
            return Err(fail(format!(
                "duplicate declaration of `{}` in the header; HNA descriptors cannot contain two functions with one name",
                raw.name
            )));
        }

        let ret =
            match resolver.classify(&raw.ret).map_err(|detail| Unsupported {
                symbol: format!("{} (return type)", raw.name),
                line: raw.line,
                detail,
            })? {
                Shape::Value(kind) => kind,
                Shape::StrPtr => Kind::Str,
                Shape::BytePtr => return Err(fail(
                    "const uint8_t* return requires an explicit length pair; not generated in v1"
                        .to_owned(),
                )),
                Shape::MutablePtr => return Err(fail(
                    "mutable pointer return requires an explicit length pair; not generated in v1"
                        .to_owned(),
                )),
            };
        if let Kind::Enum(tag) = &ret {
            enums.insert(tag.clone());
        }
        decisions.push(Decision {
            function: raw.name.clone(),
            line: raw.line,
            item: "return".to_owned(),
            c_type: raw.ret.spelling.clone(),
            mapping: ret.description(),
        });

        let mut params = Vec::new();
        let mut used_names = BTreeSet::new();
        let mut skip_length_at: Option<usize> = None;

        for (c_index, param) in raw.params.iter().enumerate() {
            if skip_length_at == Some(c_index) {
                decisions.push(Decision {
                    function: raw.name.clone(),
                    line: param.line,
                    item: format!("parameter {c_index} (`{}`)", c_name(param)),
                    c_type: param.typ.spelling.clone(),
                    mapping: "consumed as the length of the preceding bytes/str pointer".to_owned(),
                });
                continue;
            }

            let kind = classify_parameter(
                &resolver,
                raw,
                param,
                c_index,
                &mut skip_length_at,
                &mut enums,
            )
            .map_err(|detail| Unsupported {
                symbol: format!("{} (parameter {})", raw.name, c_name(param)),
                line: param.line,
                detail,
            })?;

            let name = py_name(param.name.as_deref(), c_index, &mut used_names);
            decisions.push(Decision {
                function: raw.name.clone(),
                line: param.line,
                item: format!("parameter `{name}`"),
                c_type: param.typ.spelling.clone(),
                mapping: kind.description(),
            });
            params.push(Param {
                name,
                kind,
                c_type: param.typ.spelling.clone(),
            });
        }

        let generated_index = functions.len();
        functions.push(Function {
            c_name: raw.name.clone(),
            line: raw.line,
            index: generated_index,
            ret,
            ret_c_type: raw.ret.spelling.clone(),
            params,
        });
    }

    if functions.is_empty() {
        return Err(Unsupported {
            symbol: module_name.to_owned(),
            line: 0,
            detail: "header declares no exportable functions in the supported subset".to_owned(),
        });
    }

    let mut constants = Vec::new();
    let enum_table = crate::clang::EnumTable::from_header(header);
    for tag in enums {
        if let Some(variants) = enum_table.by_tag.get(&tag) {
            constants.extend(variants.iter().cloned());
        }
    }
    constants.sort_by(|left, right| left.0.cmp(&right.0));
    constants.dedup_by(|left, right| left.0 == right.0);
    for (name, _) in &constants {
        validate_c_string("enum constant name", name).map_err(|detail| Unsupported {
            symbol: name.clone(),
            line: 0,
            detail,
        })?;
    }

    Ok(Module {
        name: module_name.to_owned(),
        provider: provider.to_owned(),
        header: header_path.to_owned(),
        functions,
        decisions,
        excluded: header.excluded.clone(),
        foreign_functions: header.foreign_functions,
        constants,
    })
}

fn c_name(param: &RawParam) -> String {
    param.name.clone().unwrap_or_else(|| "<unnamed>".to_owned())
}

/// Classify a parameter, collapsing an adjacent pointer/length pair into one
/// `Bytes` parameter and marking the length for skipping.
fn classify_parameter(
    resolver: &Resolver<'_>,
    function: &RawFunction,
    param: &RawParam,
    c_index: usize,
    skip_length_at: &mut Option<usize>,
    enums: &mut BTreeSet<String>,
) -> Result<Kind, String> {
    let shape = resolver.classify(&param.typ)?;
    let next = function.params.get(c_index + 1);
    let pair = next.map(|next| is_size_t(&next.typ)).unwrap_or(false);
    match shape {
        Shape::Value(kind) => {
            if let Kind::Enum(tag) = &kind {
                enums.insert(tag.clone());
            }
            Ok(kind)
        }
        Shape::StrPtr => {
            if pair {
                *skip_length_at = Some(c_index + 1);
                Ok(Kind::Bytes)
            } else {
                Ok(Kind::Str)
            }
        }
        Shape::BytePtr => {
            if !pair {
                return Err(
                    "const uint8_t*/unsigned char* requires an adjacent size_t length parameter"
                        .to_owned(),
                );
            }
            *skip_length_at = Some(c_index + 1);
            Ok(Kind::Bytes)
        }
        Shape::MutablePtr => Err(
            "mutable pointer output buffers are not generated in v1; pass an immutable buffer or use a scalar API"
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clang::{RawEnum, RawHeader, RawParam, RawTypedef};

    fn ty(spelling: &str) -> RawType {
        RawType {
            spelling: spelling.to_owned(),
            desugared: None,
        }
    }

    fn typedef(name: &str, spelling: &str) -> RawTypedef {
        RawTypedef {
            name: name.to_owned(),
            typ: ty(spelling),
            implicit: false,
        }
    }

    fn header(functions: Vec<RawFunction>, typedefs: Vec<RawTypedef>) -> RawHeader {
        RawHeader {
            functions,
            typedefs,
            ..RawHeader::default()
        }
    }

    fn function(name: &str, ret: &str, params: &[(&str, &str)]) -> RawFunction {
        RawFunction {
            name: name.to_owned(),
            line: 7,
            ret: ty(ret),
            params: params
                .iter()
                .map(|(name, typ)| RawParam {
                    name: Some((*name).to_owned()),
                    typ: ty(typ),
                    line: 7,
                })
                .collect(),
            variadic: false,
            storage_static: false,
            implicit: false,
        }
    }

    fn build(function: RawFunction, typedefs: Vec<RawTypedef>) -> Result<Module, Unsupported> {
        build_module(
            &header(vec![function], typedefs),
            "example._fast",
            "example 1.0",
            "lib.h",
        )
    }

    #[test]
    fn maps_integer_widths_and_ranges() {
        let module = build(
            function(
                "counts",
                "unsigned long",
                &[
                    ("a", "int8_t"),
                    ("b", "uint8_t"),
                    ("c", "short"),
                    ("d", "unsigned int"),
                    ("e", "long long"),
                    ("f", "size_t"),
                ],
            ),
            Vec::new(),
        )
        .unwrap();
        let kinds: Vec<_> = module.functions[0]
            .params
            .iter()
            .map(|param| param.kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                Kind::Int {
                    bits: 8,
                    signed: true,
                    ffi: "i8"
                },
                Kind::Int {
                    bits: 8,
                    signed: false,
                    ffi: "u8"
                },
                Kind::Int {
                    bits: 16,
                    signed: true,
                    ffi: "i16"
                },
                Kind::Int {
                    bits: 32,
                    signed: false,
                    ffi: "u32"
                },
                Kind::Int {
                    bits: 64,
                    signed: true,
                    ffi: "i64"
                },
                Kind::Int {
                    bits: 64,
                    signed: false,
                    ffi: "u64"
                },
            ]
        );
        assert_eq!(module.functions[0].ret.int_range(), Some((0, i64::MAX)));
    }

    #[test]
    fn maps_nul_terminated_string_input() {
        let module = build(
            function("greet", "void", &[("name", "const char *")]),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(module.functions[0].params[0].kind, Kind::Str);
        assert_eq!(module.functions[0].params[0].name, "name");
    }

    #[test]
    fn maps_pointer_length_pair_to_bytes() {
        let module = build(
            function(
                "digest",
                "void",
                &[("data", "const uint8_t *"), ("len", "size_t")],
            ),
            Vec::new(),
        )
        .unwrap();
        let function = &module.functions[0];
        assert_eq!(function.params.len(), 1);
        assert_eq!(function.params[0].kind, Kind::Bytes);
        assert_eq!(function.min_args(), 1);
        assert_eq!(function.max_args(), 1);
    }

    #[test]
    fn maps_const_char_length_pair_to_bytes() {
        let module = build(
            function("feed", "int", &[("text", "const char *"), ("n", "size_t")]),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(module.functions[0].params[0].kind, Kind::Bytes);
    }

    #[test]
    fn resolves_typedefs_for_mapping() {
        let module = build(
            function("severity", "int", &[("level", "Level")]),
            vec![typedef("Level", "enum Severity")],
        )
        .unwrap();
        assert_eq!(
            module.functions[0].params[0].kind,
            Kind::Enum("Severity".into())
        );
    }

    #[test]
    fn rejects_variadic_functions() {
        let mut f = function("logf", "int", &[("fmt", "const char *")]);
        f.variadic = true;
        let error = build(f, Vec::new()).unwrap_err();
        assert!(error.detail.contains("variadic"));
    }

    #[test]
    fn rejects_function_pointers() {
        let error = build(
            function("set_cb", "void", &[("cb", "void (*)(int)")]),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.detail.contains("function pointer"));
    }

    #[test]
    fn rejects_long_double() {
        let error = build(
            function("wide", "long double", &[("x", "long double")]),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.detail.contains("long double"));
    }

    #[test]
    fn rejects_mutable_output_pointer() {
        let error = build(
            function("read", "int", &[("buf", "char *"), ("n", "size_t")]),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.detail.contains("mutable"));
    }

    #[test]
    fn rejects_struct_by_value() {
        let error = build(
            function("norm", "struct vec", &[("v", "struct vec")]),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.detail.contains("struct by value"));
    }

    #[test]
    fn rejects_byte_pointer_without_length() {
        let error = build(
            function("feed", "int", &[("data", "const uint8_t *")]),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.detail.contains("size_t"));
    }

    #[test]
    fn module_name_validation_is_strict() {
        assert!(validate_module_name("pkg._fast").is_ok());
        for bad in ["", ".pkg", "pkg.", "pkg..mod", "1pkg", "pkg-mod"] {
            assert!(validate_module_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn parameter_names_are_python_safe() {
        let mut used = BTreeSet::new();
        assert_eq!(py_name(Some("class"), 0, &mut used), "_class");
        assert_eq!(py_name(Some("a-b"), 1, &mut used), "a_b");
        assert_eq!(py_name(Some(""), 2, &mut used), "arg2");
        assert_eq!(py_name(Some("x"), 3, &mut used), "x");
        assert_eq!(py_name(Some("x"), 4, &mut used), "x_2");
    }

    #[test]
    fn enum_constants_are_sorted_and_exported() {
        let mut raw = header(
            vec![function("paint", "void", &[("c", "enum Color")])],
            Vec::new(),
        );
        raw.enums.push(RawEnum {
            tag: Some("Color".into()),
            variants: vec![("RED".into(), 0), ("BLUE".into(), 2)],
        });
        let module = build_module(&raw, "paint", "paint 1.0", "paint.h").unwrap();
        assert_eq!(
            module.constants,
            vec![("BLUE".into(), 2), ("RED".into(), 0)]
        );
    }
}
