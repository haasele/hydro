//! `hydro-manifest.toml` discovery and validation for HNA artifacts.
//!
//! The normative schema is `docs/NATIVE_BINDINGS.md` §4.2. The embedded
//! descriptor stays authoritative at load time; the manifest exists so
//! installers and `hydro-bindgen --check` can inspect a distribution without
//! `dlopen`, and the loader cross-checks the two so a rebuilt binary cannot
//! silently disagree with a checked-in manifest.
//!
//! # Discovery
//!
//! The manifest is looked up in the ancestors of the canonical artifact path:
//! the directory owning the artifact first, then each parent, for at most
//! [`MAX_ANCESTOR_DEPTH`] levels. The artifact path is canonical (symlinks
//! resolved by the loader), so the searched directories contain no symlink
//! components. A `hydro-manifest.toml` that is itself a symbolic link is a
//! load error, never followed: it would escape the distribution root.
//!
//! The first manifest found is the distribution manifest:
//!
//! - a parse or schema error is a load error (never a warning, never a silent
//!   fallback to "no manifest");
//! - exactly one `[[modules]]` entry must resolve to the canonical artifact
//!   path; no match is a load error naming the manifest and the artifact.
//!
//! A directory tree with no manifest keeps the pre-manifest loader behaviour.
//!
//! # Schema
//!
//! Only the required HNA schema is accepted. The parser is deliberately
//! minimal (no dependency): comments, blank lines, bare `key = value`
//! assignments, basic (`"..."`) and literal (`'...'`) strings, unsigned
//! integers and booleans, plus `[[modules]]` headers. Anything else —
//! unknown or duplicate keys, another table, another value form, missing
//! keys, `..` or absolute artifact paths — is an error with a line number.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use hydro_hna::RUNTIME_ABI;

use crate::native::PLATFORM;

/// File name of a Hydro-native package manifest.
const MANIFEST_FILE: &str = "hydro-manifest.toml";

/// Upper bound on the ancestor walk, so a stray manifest far above the
/// artifact cannot be picked up.
const MAX_ANCESTOR_DEPTH: usize = 16;

/// Keys allowed in the top-level table.
const TOP_LEVEL_KEYS: &[&str] = &[
    "manifest_version",
    "hna_version",
    "hydro_abi_min",
    "platform",
    "provider",
];

/// Keys allowed in one `[[modules]]` entry.
const MODULE_KEYS: &[&str] = &["name", "artifact", "thread_safe"];

/// One `[[modules]]` entry.
#[derive(Debug)]
pub(crate) struct ManifestEntry {
    pub(crate) name: String,
    pub(crate) artifact: String,
    pub(crate) thread_safe: bool,
}

/// A validated `hydro-manifest.toml`.
#[derive(Debug)]
pub(crate) struct Manifest {
    pub(crate) hna_version: u32,
    pub(crate) hydro_abi_min: u64,
    pub(crate) platform: String,
    pub(crate) provider: String,
    pub(crate) modules: Vec<ManifestEntry>,
}

/// A manifest located for one artifact, with the matching entry selected.
#[derive(Debug)]
pub(crate) struct LocatedManifest {
    /// Path of the manifest file that owns the distribution.
    pub(crate) path: PathBuf,
    /// Index in [`Manifest::modules`] whose artifact is the loaded one.
    entry_index: usize,
    pub(crate) manifest: Manifest,
}

impl LocatedManifest {
    /// The `[[modules]]` entry that declares the loaded artifact.
    pub(crate) fn entry(&self) -> &ManifestEntry {
        &self.manifest.modules[self.entry_index]
    }

    fn prefix(&self) -> String {
        format!(
            "hna manifest '{}': entry '{}'",
            self.path.display(),
            self.entry().name
        )
    }
}

/// The descriptor facts a manifest entry is cross-checked against.
pub(crate) struct DescriptorFacts<'a> {
    pub(crate) module_name: &'a str,
    pub(crate) hna_version: u32,
    pub(crate) platform: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) thread_safe: bool,
    pub(crate) runtime_abi_min: u64,
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Finds the manifest for `canonical` and selects its `[[modules]]` entry.
///
/// `canonical` must be the canonical artifact path. `Ok(None)` means no
/// `hydro-manifest.toml` exists in the bounded ancestor walk; every other
/// failure is a load diagnostic naming the manifest and/or the artifact.
pub(crate) fn find_for_artifact(canonical: &Path) -> Result<Option<LocatedManifest>, String> {
    let Some(start) = canonical.parent() else {
        return Ok(None);
    };
    let mut directory = start;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        let candidate = directory.join(MANIFEST_FILE);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "hna manifest '{}' is a symbolic link; refusing to follow it",
                        candidate.display()
                    ));
                }
                if !metadata.is_file() {
                    return Err(format!(
                        "hna manifest '{}' is not a regular file",
                        candidate.display()
                    ));
                }
                let text = std::fs::read_to_string(&candidate).map_err(|error| {
                    format!(
                        "cannot read hna manifest '{}': {error}",
                        candidate.display()
                    )
                })?;
                let manifest = parse(&text, &candidate)?;
                let entry_index = select_entry(&manifest, directory, canonical, &candidate)?;
                return Ok(Some(LocatedManifest {
                    path: candidate,
                    entry_index,
                    manifest,
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect hna manifest '{}': {error}",
                    candidate.display()
                ))
            }
        }
        let Some(parent) = directory.parent() else {
            break;
        };
        directory = parent;
    }
    Ok(None)
}

/// Selects the entry whose artifact resolves exactly to `canonical`.
///
/// Every entry's artifact must exist (`docs/NATIVE_BINDINGS.md` §4.2); a
/// missing artifact is an error naming the entry, the artifact and the
/// resolved candidate path.
fn select_entry(
    manifest: &Manifest,
    root: &Path,
    canonical: &Path,
    manifest_path: &Path,
) -> Result<usize, String> {
    for (index, entry) in manifest.modules.iter().enumerate() {
        let candidate = root.join(&entry.artifact);
        match std::fs::canonicalize(&candidate) {
            Ok(resolved) => {
                if resolved == canonical {
                    return Ok(index);
                }
            }
            Err(error) => {
                return Err(format!(
                    "hna manifest '{}': module entry {index} artifact '{}' does not exist (resolved '{}'): {error}",
                    manifest_path.display(),
                    entry.artifact,
                    candidate.display()
                ))
            }
        }
    }
    Err(format!(
        "hna manifest '{}' has no [[modules]] entry for artifact '{}'",
        manifest_path.display(),
        canonical.display()
    ))
}

/// Cross-checks the embedded descriptor against the selected manifest entry.
///
/// Every mismatch is a load error naming both sides.
pub(crate) fn check_descriptor(
    found: &LocatedManifest,
    facts: &DescriptorFacts<'_>,
) -> Result<(), String> {
    let entry = found.entry();
    let prefix = found.prefix();
    if entry.name != facts.module_name {
        return Err(format!(
            "{prefix}: module_name '{}' does not match descriptor module_name '{}'",
            entry.name, facts.module_name
        ));
    }
    if found.manifest.hna_version != facts.hna_version {
        return Err(format!(
            "{prefix}: hna_version {} does not match descriptor hna_version {}",
            found.manifest.hna_version, facts.hna_version
        ));
    }
    if found.manifest.platform != facts.platform {
        return Err(format!(
            "{prefix}: platform '{}' does not match descriptor platform '{}'",
            found.manifest.platform, facts.platform
        ));
    }
    if found.manifest.provider != facts.provider {
        return Err(format!(
            "{prefix}: provider '{}' does not match descriptor provider '{}'",
            found.manifest.provider, facts.provider
        ));
    }
    if entry.thread_safe != facts.thread_safe {
        return Err(format!(
            "{prefix}: thread_safe {} does not match descriptor thread_safe {}",
            entry.thread_safe, facts.thread_safe
        ));
    }
    if facts.runtime_abi_min > found.manifest.hydro_abi_min {
        return Err(format!(
            "{prefix}: hydro_abi_min {} is below descriptor runtime_abi_min {}",
            found.manifest.hydro_abi_min, facts.runtime_abi_min
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal TOML parser
// ---------------------------------------------------------------------------

/// One parsed value of the supported schema.
#[derive(Debug)]
enum Value {
    Int(u64),
    Str(String),
    Bool(bool),
}

fn diagnostic(path: &Path, line: usize, detail: impl std::fmt::Display) -> String {
    format!("hna manifest '{}': line {line}: {detail}", path.display())
}

fn manifest_error(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("hna manifest '{}': {detail}", path.display())
}

fn entry_error(path: &Path, label: Option<&str>, detail: impl std::fmt::Display) -> String {
    match label {
        Some(label) => manifest_error(path, format!("{label}: {detail}")),
        None => manifest_error(path, detail),
    }
}

/// Parses and validates manifest text. `path` is only used in diagnostics.
fn parse(text: &str, path: &Path) -> Result<Manifest, String> {
    let mut top_level: HashMap<String, Value> = HashMap::new();
    let mut modules: Vec<HashMap<String, Value>> = Vec::new();
    let mut in_module = false;

    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line == "[[modules]]" {
                modules.push(HashMap::new());
                in_module = true;
                continue;
            }
            return Err(diagnostic(
                path,
                line_number,
                format!("unsupported table header '{line}'"),
            ));
        }
        let (key, value) =
            split_assignment(line).map_err(|detail| diagnostic(path, line_number, detail))?;
        let key = key.trim();
        if !is_bare_key(key) {
            return Err(diagnostic(
                path,
                line_number,
                format!("invalid key '{key}'"),
            ));
        }
        let allowed = if in_module {
            MODULE_KEYS
        } else {
            TOP_LEVEL_KEYS
        };
        if !allowed.contains(&key) {
            return Err(diagnostic(
                path,
                line_number,
                format!("unknown key '{key}'"),
            ));
        }
        let value = parse_value(value, key, path, line_number)?;
        let table = if in_module {
            match modules.last_mut() {
                Some(table) => table,
                None => {
                    return Err(diagnostic(
                        path,
                        line_number,
                        "module entry has no [[modules]] header".to_string(),
                    ))
                }
            }
        } else {
            &mut top_level
        };
        if table.contains_key(key) {
            return Err(diagnostic(
                path,
                line_number,
                format!("duplicate key '{key}'"),
            ));
        }
        table.insert(key.to_string(), value);
    }

    compile(top_level, modules, path)
}

/// Removes a `#` comment while respecting string literals.
fn strip_comment(line: &str) -> &str {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for (offset, ch) in line.char_indices() {
        if in_basic {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if ch == '\'' {
                in_literal = false;
            }
        } else {
            match ch {
                '"' => in_basic = true,
                '\'' => in_literal = true,
                '#' => return &line[..offset],
                _ => {}
            }
        }
    }
    line
}

/// Splits `key = value` at the first `=` outside a string literal.
fn split_assignment(line: &str) -> Result<(&str, &str), &'static str> {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for (offset, ch) in line.char_indices() {
        if in_basic {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_basic = false;
            }
        } else if in_literal {
            if ch == '\'' {
                in_literal = false;
            }
        } else {
            match ch {
                '"' => in_basic = true,
                '\'' => in_literal = true,
                '=' => return Ok((&line[..offset], &line[offset + 1..])),
                _ => {}
            }
        }
    }
    Err("expected 'key = value'")
}

fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn parse_value(raw: &str, key: &str, path: &Path, line: usize) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(diagnostic(
            path,
            line,
            format!("missing value for key '{key}'"),
        ));
    }
    if let Some(rest) = raw.strip_prefix('"') {
        return parse_basic_string(rest, key, path, line);
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        return parse_literal_string(rest, key, path, line);
    }
    if raw == "true" {
        return Ok(Value::Bool(true));
    }
    if raw == "false" {
        return Ok(Value::Bool(false));
    }
    if raw.chars().all(|ch| ch.is_ascii_digit()) {
        return raw.parse::<u64>().map(Value::Int).map_err(|_| {
            diagnostic(
                path,
                line,
                format!("integer literal out of range for key '{key}'"),
            )
        });
    }
    Err(diagnostic(
        path,
        line,
        format!("unsupported value for key '{key}'"),
    ))
}

fn trailing_after_value(rest: &str, key: &str, path: &Path, line: usize) -> Result<(), String> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(diagnostic(
            path,
            line,
            format!("trailing characters after value for key '{key}'"),
        ))
    }
}

/// Parses a TOML basic string body (after the opening quote).
fn parse_basic_string(rest: &str, key: &str, path: &Path, line: usize) -> Result<Value, String> {
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '"' => {
                trailing_after_value(&rest[offset + 1..], key, path, line)?;
                return Ok(Value::Str(out));
            }
            '\\' => {
                let Some((_, escape)) = chars.next() else {
                    return Err(diagnostic(
                        path,
                        line,
                        format!("unterminated string for key '{key}'"),
                    ));
                };
                match escape {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'u' => {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            let Some((_, digit)) = chars.next() else {
                                return Err(diagnostic(
                                    path,
                                    line,
                                    format!("unterminated \\u escape for key '{key}'"),
                                ));
                            };
                            let Some(value) = digit.to_digit(16) else {
                                return Err(diagnostic(
                                    path,
                                    line,
                                    format!("invalid \\u escape '\\u{digit}' for key '{key}'"),
                                ));
                            };
                            code = code * 16 + value;
                        }
                        let Some(character) = char::from_u32(code) else {
                            return Err(diagnostic(
                                path,
                                line,
                                format!("invalid \\u escape '\\u{code:04X}' for key '{key}'"),
                            ));
                        };
                        out.push(character);
                    }
                    other => {
                        return Err(diagnostic(
                            path,
                            line,
                            format!("invalid escape '\\{other}' for key '{key}'"),
                        ))
                    }
                }
            }
            character => out.push(character),
        }
    }
    Err(diagnostic(
        path,
        line,
        format!("unterminated string for key '{key}'"),
    ))
}

/// Parses a TOML literal string body (after the opening quote).
fn parse_literal_string(rest: &str, key: &str, path: &Path, line: usize) -> Result<Value, String> {
    for (offset, ch) in rest.char_indices() {
        if ch == '\'' {
            trailing_after_value(&rest[offset + 1..], key, path, line)?;
            return Ok(Value::Str(rest[..offset].to_string()));
        }
    }
    Err(diagnostic(
        path,
        line,
        format!("unterminated string for key '{key}'"),
    ))
}

// ---------------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------------

fn take_int(
    table: &mut HashMap<String, Value>,
    key: &str,
    path: &Path,
    label: Option<&str>,
) -> Result<u64, String> {
    match table.remove(key) {
        None => Err(entry_error(
            path,
            label,
            format!("missing required key '{key}'"),
        )),
        Some(Value::Int(number)) => Ok(number),
        Some(_) => Err(entry_error(
            path,
            label,
            format!("key '{key}' must be an integer"),
        )),
    }
}

fn take_string(
    table: &mut HashMap<String, Value>,
    key: &str,
    path: &Path,
    label: Option<&str>,
) -> Result<String, String> {
    match table.remove(key) {
        None => Err(entry_error(
            path,
            label,
            format!("missing required key '{key}'"),
        )),
        Some(Value::Str(text)) => Ok(text),
        Some(_) => Err(entry_error(
            path,
            label,
            format!("key '{key}' must be a string"),
        )),
    }
}

fn take_bool(
    table: &mut HashMap<String, Value>,
    key: &str,
    path: &Path,
    label: Option<&str>,
) -> Result<bool, String> {
    match table.remove(key) {
        None => Err(entry_error(
            path,
            label,
            format!("missing required key '{key}'"),
        )),
        Some(Value::Bool(flag)) => Ok(flag),
        Some(_) => Err(entry_error(
            path,
            label,
            format!("key '{key}' must be a boolean"),
        )),
    }
}

fn compile(
    mut top_level: HashMap<String, Value>,
    modules: Vec<HashMap<String, Value>>,
    path: &Path,
) -> Result<Manifest, String> {
    let manifest_version = take_int(&mut top_level, "manifest_version", path, None)?;
    let hna_version = take_int(&mut top_level, "hna_version", path, None)?;
    let hydro_abi_min = take_int(&mut top_level, "hydro_abi_min", path, None)?;
    let platform = take_string(&mut top_level, "platform", path, None)?;
    let provider = take_string(&mut top_level, "provider", path, None)?;

    if manifest_version != 1 {
        return Err(manifest_error(
            path,
            format!("manifest_version {manifest_version}, runtime supports 1"),
        ));
    }
    if hna_version != 1 {
        return Err(manifest_error(
            path,
            format!("hna_version {hna_version}, runtime supports 1"),
        ));
    }
    let supported_abi = 1..=u64::from(RUNTIME_ABI);
    if !supported_abi.contains(&hydro_abi_min) {
        return Err(manifest_error(
            path,
            format!("hydro_abi_min {hydro_abi_min}, runtime supports 1"),
        ));
    }
    if platform != PLATFORM {
        return Err(manifest_error(
            path,
            format!("platform '{platform}', runtime platform '{PLATFORM}'"),
        ));
    }
    if modules.is_empty() {
        return Err(manifest_error(path, "missing required [[modules]] entry"));
    }

    let mut names: HashSet<String> = HashSet::new();
    let mut artifacts: HashSet<String> = HashSet::new();
    let mut entries = Vec::with_capacity(modules.len());
    for (index, mut table) in modules.into_iter().enumerate() {
        let label = format!("module entry {index}");
        let name = take_string(&mut table, "name", path, Some(&label))?;
        let artifact = take_string(&mut table, "artifact", path, Some(&label))?;
        let thread_safe = take_bool(&mut table, "thread_safe", path, Some(&label))?;
        if name.is_empty() {
            return Err(entry_error(path, Some(&label), "name is empty"));
        }
        if artifact.is_empty() {
            return Err(entry_error(path, Some(&label), "artifact is empty"));
        }
        let artifact_path = Path::new(&artifact);
        if artifact_path.is_absolute() {
            return Err(entry_error(
                path,
                Some(&label),
                format!("artifact '{artifact}' is absolute"),
            ));
        }
        if artifact_path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(entry_error(
                path,
                Some(&label),
                format!("artifact '{artifact}' contains '..'"),
            ));
        }
        if !names.insert(name.clone()) {
            return Err(manifest_error(
                path,
                format!("duplicate module name '{name}'"),
            ));
        }
        if !artifacts.insert(artifact.clone()) {
            return Err(manifest_error(
                path,
                format!("duplicate module artifact '{artifact}'"),
            ));
        }
        entries.push(ManifestEntry {
            name,
            artifact,
            thread_safe,
        });
    }

    Ok(Manifest {
        hna_version: hna_version as u32,
        hydro_abi_min,
        platform,
        provider,
        modules: entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// Unique directory removed on drop, so tests do not depend on a shared
    /// `$TMPDIR` layout.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("hydro-hna-manifest-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temporary directory");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent directory");
            }
            std::fs::write(&path, contents).expect("write temporary file");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn valid_manifest(artifact: &str) -> String {
        format!(
            "# @generated by hydro-bindgen; do not edit.\n\
             manifest_version = 1\n\
             hna_version = 1\n\
             hydro_abi_min = 1\n\
             platform = \"linux-x86_64\"\n\
             provider = \"example-native 0.4.0\"\n\
             \n\
             [[modules]]\n\
             name = \"example._fast\"\n\
             artifact = \"{artifact}\"\n\
             thread_safe = false\n"
        )
    }

    fn parse_text(text: &str) -> Result<Manifest, String> {
        parse(text, Path::new("hydro-manifest.toml"))
    }

    fn facts() -> DescriptorFacts<'static> {
        DescriptorFacts {
            module_name: "example._fast",
            hna_version: 1,
            platform: PLATFORM,
            provider: "example-native 0.4.0",
            thread_safe: false,
            runtime_abi_min: 1,
        }
    }

    fn locate(dir: &TempDir, artifact: &str) -> LocatedManifest {
        let path = dir.write(artifact, "");
        let canonical = std::fs::canonicalize(path).expect("canonical artifact");
        find_for_artifact(&canonical)
            .expect("manifest loads")
            .expect("manifest found")
    }

    #[test]
    fn locates_and_selects_the_matching_artifact() {
        let dir = TempDir::new();
        dir.write(MANIFEST_FILE, &valid_manifest("example/_fast.hna.so"));
        let found = locate(&dir, "example/_fast.hna.so");
        assert_eq!(found.entry().name, "example._fast");
        assert_eq!(found.entry().artifact, "example/_fast.hna.so");
        assert!(!found.entry().thread_safe);
        assert_eq!(found.manifest.provider, "example-native 0.4.0");
        assert_eq!(found.manifest.hydro_abi_min, 1);
        assert!(check_descriptor(&found, &facts()).is_ok());
    }

    #[test]
    fn missing_manifest_is_not_an_error() {
        let dir = TempDir::new();
        let path = dir.write("pkg/nested/_fast.hna.so", "");
        let canonical = std::fs::canonicalize(path).expect("canonical artifact");
        assert!(find_for_artifact(&canonical)
            .expect("no manifest is not an error")
            .is_none());
    }

    #[test]
    fn parses_comments_literal_strings_and_escapes() {
        let text = valid_manifest("example/_fast.hna.so")
            .replace(
                "provider = \"example-native 0.4.0\"",
                "provider = 'literal \\u0041 # not a comment'",
            )
            .replace(
                "platform = \"linux-x86_64\"",
                "platform = \"linux-x86_64\" # host platform",
            );
        let manifest = parse_text(&text).expect("valid manifest");
        assert_eq!(manifest.provider, "literal \\u0041 # not a comment");
        assert_eq!(manifest.platform, PLATFORM);

        let escaped = valid_manifest("example/_fast.hna.so").replace(
            "provider = \"example-native 0.4.0\"",
            "provider = \"acme\\t1.0 \\\"quoted\\\" \\u0041\"",
        );
        let manifest = parse_text(&escaped).expect("valid escapes");
        assert_eq!(manifest.provider, "acme\t1.0 \"quoted\" A");
    }

    #[test]
    fn missing_required_keys_are_errors() {
        let text = valid_manifest("example/_fast.hna.so")
            .replace("provider = \"example-native 0.4.0\"\n", "");
        let error = parse_text(&text).unwrap_err();
        assert!(error.contains("missing required key 'provider'"), "{error}");

        let text = valid_manifest("example/_fast.hna.so").replace("thread_safe = false\n", "");
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("module entry 0: missing required key 'thread_safe'"),
            "{error}"
        );

        let error = parse_text(
            "manifest_version = 1\nhna_version = 1\nhydro_abi_min = 1\nplatform = \"linux-x86_64\"\nprovider = \"x\"\n",
        )
        .unwrap_err();
        assert!(
            error.contains("missing required [[modules]] entry"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_and_tables_are_errors() {
        let text = format!(
            "{}license = \"MIT\"\n",
            valid_manifest("example/_fast.hna.so")
        );
        let error = parse_text(&text).unwrap_err();
        assert!(error.contains("unknown key 'license'"), "{error}");

        let text = valid_manifest("example/_fast.hna.so").replace("thread_safe = false", "abi = 1");
        let error = parse_text(&text).unwrap_err();
        assert!(error.contains("unknown key 'abi'"), "{error}");

        let text = valid_manifest("example/_fast.hna.so").replace("[[modules]]", "[package]");
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("unsupported table header '[package]'"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_keys_and_modules_are_errors() {
        let text = valid_manifest("example/_fast.hna.so").replace(
            "platform = \"linux-x86_64\"\n",
            "platform = \"linux-x86_64\"\nplatform = \"linux-x86_64\"\n",
        );
        let error = parse_text(&text).unwrap_err();
        assert!(error.contains("duplicate key 'platform'"), "{error}");

        let text = format!(
            "{}\n[[modules]]\nname = \"example._fast\"\nartifact = \"example/other.hna.so\"\nthread_safe = false\n",
            valid_manifest("example/_fast.hna.so")
        );
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("duplicate module name 'example._fast'"),
            "{error}"
        );

        let text = format!(
            "{}\n[[modules]]\nname = \"example._slow\"\nartifact = \"example/_fast.hna.so\"\nthread_safe = false\n",
            valid_manifest("example/_fast.hna.so")
        );
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("duplicate module artifact 'example/_fast.hna.so'"),
            "{error}"
        );
    }

    #[test]
    fn artifact_paths_must_be_relative_and_confined() {
        let text = valid_manifest("../escape.hna.so");
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("artifact '../escape.hna.so' contains '..'"),
            "{error}"
        );

        let text = valid_manifest("/tmp/escape.hna.so");
        let error = parse_text(&text).unwrap_err();
        assert!(
            error.contains("artifact '/tmp/escape.hna.so' is absolute"),
            "{error}"
        );

        let text = valid_manifest("example/../../escape.hna.so");
        let error = parse_text(&text).unwrap_err();
        assert!(error.contains("contains '..'"), "{error}");
    }

    #[test]
    fn schema_versions_and_platform_are_checked() {
        let error = parse_text(
            &valid_manifest("x.hna.so").replace("manifest_version = 1", "manifest_version = 2"),
        )
        .unwrap_err();
        assert!(
            error.contains("manifest_version 2, runtime supports 1"),
            "{error}"
        );

        let error =
            parse_text(&valid_manifest("x.hna.so").replace("hna_version = 1", "hna_version = 3"))
                .unwrap_err();
        assert!(
            error.contains("hna_version 3, runtime supports 1"),
            "{error}"
        );

        let error = parse_text(
            &valid_manifest("x.hna.so").replace("hydro_abi_min = 1", "hydro_abi_min = 2"),
        )
        .unwrap_err();
        assert!(
            error.contains("hydro_abi_min 2, runtime supports 1"),
            "{error}"
        );

        let error = parse_text(
            &valid_manifest("x.hna.so")
                .replace("platform = \"linux-x86_64\"", "platform = \"darwin-arm64\""),
        )
        .unwrap_err();
        assert!(
            error.contains("platform 'darwin-arm64', runtime platform 'linux-x86_64'"),
            "{error}"
        );

        let error = parse_text(
            &valid_manifest("x.hna.so").replace("thread_safe = false", "thread_safe = 1"),
        )
        .unwrap_err();
        assert!(
            error.contains("key 'thread_safe' must be a boolean"),
            "{error}"
        );

        let error = parse_text(
            &valid_manifest("x.hna.so")
                .replace("provider = \"example-native 0.4.0\"", "provider = example"),
        )
        .unwrap_err();
        assert!(
            error.contains("unsupported value for key 'provider'"),
            "{error}"
        );
    }

    #[test]
    fn unterminated_strings_are_errors() {
        let error = parse_text(&valid_manifest("x.hna.so").replace(
            "provider = \"example-native 0.4.0\"",
            "provider = \"example-native",
        ))
        .unwrap_err();
        assert!(error.contains("unterminated string"), "{error}");

        let error = parse_text(&valid_manifest("x.hna.so").replace(
            "provider = \"example-native 0.4.0\"",
            "provider = \"bad\\qescape\"",
        ))
        .unwrap_err();
        assert!(error.contains("invalid escape '\\q'"), "{error}");
    }

    #[test]
    fn manifest_without_a_matching_entry_names_both_sides() {
        let dir = TempDir::new();
        dir.write(MANIFEST_FILE, &valid_manifest("example/_slow.hna.so"));
        let fast = dir.write("example/_fast.hna.so", "");
        dir.write("example/_slow.hna.so", "");
        let canonical = std::fs::canonicalize(fast).expect("canonical artifact");
        let error = find_for_artifact(&canonical).unwrap_err();
        assert!(
            error.contains("has no [[modules]] entry for artifact"),
            "{error}"
        );
        assert!(error.contains("_fast.hna.so"), "{error}");
        assert!(error.contains("hydro-manifest.toml"), "{error}");
    }

    #[test]
    fn declared_artifact_must_exist() {
        let dir = TempDir::new();
        dir.write(MANIFEST_FILE, &valid_manifest("example/absent.hna.so"));
        let fast = dir.write("example/_fast.hna.so", "");
        let canonical = std::fs::canonicalize(fast).expect("canonical artifact");
        let error = find_for_artifact(&canonical).unwrap_err();
        assert!(
            error.contains("module entry 0 artifact 'example/absent.hna.so' does not exist"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_manifest_is_refused() {
        let dir = TempDir::new();
        let target = dir.write("elsewhere.toml", &valid_manifest("pkg/_fast.hna.so"));
        let link = dir.path().join(MANIFEST_FILE);
        std::os::unix::fs::symlink(target, &link).expect("create manifest symlink");
        let fast = dir.write("pkg/_fast.hna.so", "");
        let canonical = std::fs::canonicalize(fast).expect("canonical artifact");
        let error = find_for_artifact(&canonical).unwrap_err();
        assert!(error.contains("is a symbolic link"), "{error}");
    }

    #[test]
    fn descriptor_mismatches_name_both_sides() {
        let dir = TempDir::new();
        dir.write(MANIFEST_FILE, &valid_manifest("example/_fast.hna.so"));
        let found = locate(&dir, "example/_fast.hna.so");

        let wrong = DescriptorFacts {
            module_name: "example.other",
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains(
                "module_name 'example._fast' does not match descriptor module_name 'example.other'"
            ),
            "{error}"
        );

        let wrong = DescriptorFacts {
            hna_version: 2,
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains("hna_version 1 does not match descriptor hna_version 2"),
            "{error}"
        );

        let wrong = DescriptorFacts {
            platform: "darwin-arm64",
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains(
                "platform 'linux-x86_64' does not match descriptor platform 'darwin-arm64'"
            ),
            "{error}"
        );

        let wrong = DescriptorFacts {
            provider: "other 1.0",
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains(
                "provider 'example-native 0.4.0' does not match descriptor provider 'other 1.0'"
            ),
            "{error}"
        );

        let wrong = DescriptorFacts {
            thread_safe: true,
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains("thread_safe false does not match descriptor thread_safe true"),
            "{error}"
        );

        let wrong = DescriptorFacts {
            runtime_abi_min: 2,
            ..facts()
        };
        let error = check_descriptor(&found, &wrong).unwrap_err();
        assert!(
            error.contains("hydro_abi_min 1 is below descriptor runtime_abi_min 2"),
            "{error}"
        );
    }
}
