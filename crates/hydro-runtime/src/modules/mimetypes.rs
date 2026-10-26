//! `mimetypes`: mapping between filenames and MIME types.
//!
//! The datastore is process-global and mutable through `add_type()`, mirroring
//! CPython's module-level tables. No external `mime.types` files are read:
//! `init()` only makes sure the built-in defaults exist, and the constants are
//! exported as fresh dictionaries so callers cannot corrupt the store by
//! mutating them.

use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

/// `(extension, type)` pairs of CPython's strict `types_map`. Preferred
/// extensions of a type come first so `guess_extension()` matches CPython.
const STRICT_TYPES: &[(&str, &str)] = &[
    (".js", "text/javascript"),
    (".mjs", "text/javascript"),
    (".epub", "application/epub+zip"),
    (".gz", "application/gzip"),
    (".json", "application/json"),
    (".webmanifest", "application/manifest+json"),
    (".doc", "application/msword"),
    (".dot", "application/msword"),
    (".bin", "application/octet-stream"),
    (".a", "application/octet-stream"),
    (".dll", "application/octet-stream"),
    (".exe", "application/octet-stream"),
    (".o", "application/octet-stream"),
    (".obj", "application/octet-stream"),
    (".so", "application/octet-stream"),
    (".pdf", "application/pdf"),
    (".ps", "application/postscript"),
    (".ai", "application/postscript"),
    (".eps", "application/postscript"),
    (".xls", "application/vnd.ms-excel"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (".odt", "application/vnd.oasis.opendocument.text"),
    (".ods", "application/vnd.oasis.opendocument.spreadsheet"),
    (".odp", "application/vnd.oasis.opendocument.presentation"),
    (
        ".pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    (
        ".xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        ".docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (".rar", "application/vnd.rar"),
    (".wasm", "application/wasm"),
    (".7z", "application/x-7z-compressed"),
    (".bz2", "application/x-bzip2"),
    (".deb", "application/x-debian-package"),
    (".php", "application/x-httpd-php"),
    (".pyc", "application/x-python-code"),
    (".rpm", "application/x-rpm"),
    (".sh", "application/x-sh"),
    (".tar", "application/x-tar"),
    (".tex", "application/x-tex"),
    (".xml", "application/xml"),
    (".xsl", "application/xml"),
    (".zip", "application/zip"),
    (".aac", "audio/aac"),
    (".flac", "audio/flac"),
    (".m4a", "audio/mp4"),
    (".mp3", "audio/mpeg"),
    (".mp2", "audio/mpeg"),
    (".ogg", "audio/ogg"),
    (".opus", "audio/opus"),
    (".wav", "audio/vnd.wave"),
    (".aif", "audio/x-aiff"),
    (".aiff", "audio/x-aiff"),
    (".otf", "font/otf"),
    (".ttf", "font/ttf"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
    (".avif", "image/avif"),
    (".bmp", "image/bmp"),
    (".gif", "image/gif"),
    (".jpg", "image/jpeg"),
    (".jpe", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".png", "image/png"),
    (".svg", "image/svg+xml"),
    (".tiff", "image/tiff"),
    (".tif", "image/tiff"),
    (".ico", "image/vnd.microsoft.icon"),
    (".webp", "image/webp"),
    (".eml", "message/rfc822"),
    (".mhtml", "message/rfc822"),
    (".css", "text/css"),
    (".csv", "text/csv"),
    (".html", "text/html"),
    (".htm", "text/html"),
    (".md", "text/markdown"),
    (".markdown", "text/markdown"),
    (".txt", "text/plain"),
    (".bat", "text/plain"),
    (".c", "text/plain"),
    (".h", "text/plain"),
    (".ksh", "text/plain"),
    (".pl", "text/plain"),
    (".srt", "text/plain"),
    (".rtf", "text/rtf"),
    (".tsv", "text/tab-separated-values"),
    (".vtt", "text/vtt"),
    (".py", "text/x-python"),
    (".rst", "text/x-rst"),
    (".vcf", "text/x-vcard"),
    (".mp4", "video/mp4"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
    (".ogv", "video/ogg"),
    (".mov", "video/quicktime"),
    (".webm", "video/webm"),
    (".avi", "video/vnd.avi"),
];

/// CPython's non-standard `common_types` (used when `strict=False`).
const COMMON_TYPES: &[(&str, &str)] = &[
    (".rtf", "application/rtf"),
    (".apk", "application/vnd.android.package-archive"),
    (".midi", "audio/midi"),
    (".mid", "audio/midi"),
    (".jpg", "image/jpg"),
    (".pict", "image/pict"),
    (".pct", "image/pict"),
    (".pic", "image/pict"),
    (".xul", "text/xul"),
];

/// `suffix_map`: compound suffixes rewritten before the type lookup.
const SUFFIX_MAP: &[(&str, &str)] = &[
    (".svgz", ".svg.gz"),
    (".tgz", ".tar.gz"),
    (".taz", ".tar.gz"),
    (".tz", ".tar.gz"),
    (".tbz2", ".tar.bz2"),
    (".txz", ".tar.xz"),
];

/// `encodings_map`: compression suffixes (case sensitive, like CPython).
const ENCODINGS_MAP: &[(&str, &str)] = &[
    (".gz", "gzip"),
    (".Z", "compress"),
    (".bz2", "bzip2"),
    (".xz", "xz"),
    (".br", "br"),
];

struct MimeDb {
    strict: Vec<(String, String)>,
    common: Vec<(String, String)>,
}

impl MimeDb {
    fn new() -> Self {
        Self {
            strict: STRICT_TYPES
                .iter()
                .map(|&(ext, kind)| (ext.to_string(), kind.to_string()))
                .collect(),
            common: COMMON_TYPES
                .iter()
                .map(|&(ext, kind)| (ext.to_string(), kind.to_string()))
                .collect(),
        }
    }

    /// Adds or replaces one extension in the selected table.
    fn add(&mut self, kind: &str, ext: &str, strict: bool) {
        let table = if strict {
            &mut self.strict
        } else {
            &mut self.common
        };
        if let Some(entry) = table.iter_mut().find(|(existing, _)| existing == ext) {
            entry.1 = kind.to_string();
        } else {
            table.push((ext.to_string(), kind.to_string()));
        }
    }

    fn lookup(&self, ext: &str, strict: bool) -> Option<&str> {
        let table = if strict { &self.strict } else { &self.common };
        table
            .iter()
            .find(|(existing, _)| existing == ext)
            .map(|(_, kind)| kind.as_str())
    }

    /// Extensions mapped to `kind`, strict entries first. The type comparison
    /// is case-insensitive like CPython's `guess_all_extensions`.
    fn extensions(&self, kind: &str, strict: bool) -> Vec<String> {
        let mut result: Vec<String> = self
            .strict
            .iter()
            .filter(|(_, existing)| existing.eq_ignore_ascii_case(kind))
            .map(|(ext, _)| ext.clone())
            .collect();
        if !strict {
            for (ext, existing) in &self.common {
                if existing.eq_ignore_ascii_case(kind) && !result.iter().any(|known| known == ext) {
                    result.push(ext.clone());
                }
            }
        }
        result
    }
}

static DB: LazyLock<Mutex<MimeDb>> = LazyLock::new(|| Mutex::new(MimeDb::new()));

fn database() -> MutexGuard<'static, MimeDb> {
    DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn string_argument(raw: i64, name: &str) -> Result<String, i64> {
    match value::as_str(raw) {
        Some(text) => Ok(text.to_string()),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name} must be str, not {}", value::type_name(raw)),
        )),
    }
}

fn strict_argument(args: &[i64], index: usize) -> bool {
    match args.get(index) {
        Some(&raw) => raw != NONE && value::truthy(raw),
        None => true,
    }
}

/// Drops a URL query/fragment and, for an absolute URL, the authority so only
/// the path remains. Plain paths without `?`/`#` are returned unchanged.
fn url_path(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let url = &url[..end];
    match url.find("://") {
        Some(position) if position > 0 => {
            let rest = &url[position + 3..];
            match rest.find('/') {
                Some(slash) => &rest[slash..],
                // `http://example.com` has no filename suffix.
                None => "",
            }
        }
        _ => url,
    }
}

/// `posixpath.splitext`: splits the extension of the final path component.
/// A leading dot is not an extension (`.bashrc` has none).
fn split_extension(path: &str) -> (&str, &str) {
    let start = path.rfind('/').map_or(0, |index| index + 1);
    match path[start..].rfind('.') {
        Some(0) | None => (path, ""),
        Some(index) => path.split_at(start + index),
    }
}

/// Splits a filename into its type extension and compression encoding,
/// applying `suffix_map` and `encodings_map` in CPython's order.
fn guess_file(path: &str, strict: bool) -> (Option<String>, Option<String>) {
    let (base, ext) = split_extension(path);
    let mut base = base.to_string();
    let mut ext = ext.to_string();
    // Rewrite compound suffixes; `suffix_map` lookup is case-insensitive.
    let mut guard = 0;
    while let Some(mapped) = SUFFIX_MAP
        .iter()
        .find(|(suffix, _)| suffix.eq_ignore_ascii_case(ext.as_str()))
        .map(|(_, target)| *target)
    {
        let combined = format!("{base}{mapped}");
        let (new_base, new_ext) = split_extension(&combined);
        base = new_base.to_string();
        ext = new_ext.to_string();
        guard += 1;
        if guard > SUFFIX_MAP.len() {
            break;
        }
    }
    // The encoding lookup is case sensitive, like CPython's map.
    let encoding = ENCODINGS_MAP
        .iter()
        .find(|(suffix, _)| *suffix == ext.as_str())
        .map(|(_, encoding)| (*encoding).to_string());
    if encoding.is_some() {
        // Strip the compression suffix and inspect the previous extension.
        let (_, previous_ext) = split_extension(&base);
        ext = previous_ext.to_string();
    }
    let lowered = ext.to_ascii_lowercase();
    let database = database();
    if let Some(kind) = database.lookup(&lowered, true) {
        return (Some(kind.to_string()), encoding);
    }
    if !strict {
        if let Some(kind) = database.lookup(&lowered, false) {
            return (Some(kind.to_string()), encoding);
        }
    }
    (None, encoding)
}

/// `guess_type(url, strict=True)`: `(type, encoding)` tuple.
fn guess_type(url: &str, strict: bool) -> i64 {
    let (kind, encoding) = guess_file(url_path(url), strict);
    let kind = kind.map_or(NONE, value::str_alloc);
    let encoding = encoding.map_or(NONE, value::str_alloc);
    value::tuple_new(vec![kind, encoding])
}

fn make_dict<I>(pairs: I) -> i64
where
    I: Iterator<Item = (String, String)>,
{
    let entries = pairs
        .map(|(key, item)| (value::str_alloc(key), value::str_alloc(item)))
        .collect();
    value::dict_new(entries)
}

fn static_dict(pairs: &'static [(&'static str, &'static str)]) -> i64 {
    make_dict(
        pairs
            .iter()
            .map(|&(key, item)| (key.to_string(), item.to_string())),
    )
}

/// Module constants. Each call returns a fresh dictionary reflecting the
/// current process-global table (so `add_type` results are visible).
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "types_map" => {
            let entries = database().strict.clone();
            make_dict(entries.into_iter())
        }
        "common_types" => {
            let entries = database().common.clone();
            make_dict(entries.into_iter())
        }
        "encodings_map" => static_dict(ENCODINGS_MAP),
        "suffix_map" => static_dict(SUFFIX_MAP),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'mimetypes' has no attribute '{name}'"),
        ),
    }
}

/// Module-level dispatch for `mimetypes`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "guess_type" => {
            if args.is_empty() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_type() missing required argument: 'url'",
                );
            }
            let url = match string_argument(args[0], "url") {
                Ok(url) => url,
                Err(error) => return error,
            };
            guess_type(&url, strict_argument(args, 1))
        }
        "guess_all_extensions" => {
            if args.is_empty() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_all_extensions() missing required argument: 'type'",
                );
            }
            let kind = match string_argument(args[0], "type") {
                Ok(kind) => kind,
                Err(error) => return error,
            };
            let extensions = database().extensions(&kind, strict_argument(args, 1));
            let values = extensions.into_iter().map(value::str_alloc).collect();
            value::list_new(values)
        }
        "guess_extension" => {
            if args.is_empty() {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_extension() missing required argument: 'type'",
                );
            }
            let kind = match string_argument(args[0], "type") {
                Ok(kind) => kind,
                Err(error) => return error,
            };
            let strict = strict_argument(args, 1);
            match database().extensions(&kind, strict).into_iter().next() {
                Some(ext) => value::str_alloc(ext),
                None => NONE,
            }
        }
        "add_type" => {
            if args.len() < 2 {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "add_type() missing required argument(s)",
                );
            }
            let kind = match string_argument(args[0], "type") {
                Ok(kind) => kind,
                Err(error) => return error,
            };
            let ext = match string_argument(args[1], "ext") {
                Ok(ext) => ext,
                Err(error) => return error,
            };
            if !kind.is_empty() {
                database().add(&kind, &ext, strict_argument(args, 2));
            }
            NONE
        }
        "init" => NONE,
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'mimetypes' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> i64 {
        value::str_from_bytes(value.as_bytes())
    }

    fn guess(url: &str, strict: bool) -> (Option<String>, Option<String>) {
        let result = call("guess_type", &[text(url), value::bool_value(strict)]);
        let items = value::as_tuple(result).expect("tuple");
        let kind = value::as_str(items[0]).map(str::to_string);
        let encoding = value::as_str(items[1]).map(str::to_string);
        (kind, encoding)
    }

    fn extensions(kind: &str, strict: bool) -> Vec<String> {
        let result = call(
            "guess_all_extensions",
            &[text(kind), value::bool_value(strict)],
        );
        value::as_list(result)
            .expect("list")
            .iter()
            .map(|item| value::as_str(*item).expect("str").to_string())
            .collect()
    }

    #[test]
    fn guesses_common_types_and_compression() {
        assert_eq!(guess("index.html", true).0.as_deref(), Some("text/html"));
        assert_eq!(
            guess("data.JSON", true).0.as_deref(),
            Some("application/json")
        );
        assert_eq!(
            guess("archive.tar.gz", true),
            (
                Some("application/x-tar".to_string()),
                Some("gzip".to_string())
            )
        );
        assert_eq!(
            guess("archive.tgz", true),
            (
                Some("application/x-tar".to_string()),
                Some("gzip".to_string())
            )
        );
        assert_eq!(guess("notes.unknown", true), (None, None));
        assert_eq!(
            guess("notes.unknown.gz", true),
            (None, Some("gzip".to_string()))
        );
        assert_eq!(guess("file.PNG", true).0.as_deref(), Some("image/png"));
    }

    #[test]
    fn strips_url_query_and_fragment() {
        assert_eq!(
            guess("https://example.com/path/photo.jpeg?x=1#frag", true)
                .0
                .as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            guess("photo.png?download=1", true).0.as_deref(),
            Some("image/png")
        );
        assert_eq!(guess("https://example.com", true), (None, None));
    }

    #[test]
    fn strict_and_common_tables_differ() {
        assert_eq!(guess("photo.jpg", true).0.as_deref(), Some("image/jpeg"));
        assert_eq!(guess("photo.jpg", false).0.as_deref(), Some("image/jpeg"));
        assert_eq!(guess("animation.mid", true).0.as_deref(), None);
        assert_eq!(
            guess("animation.mid", false).0.as_deref(),
            Some("audio/midi")
        );
        assert_eq!(extensions("image/jpg", true), Vec::<String>::new());
        assert_eq!(extensions("image/jpg", false), vec![".jpg".to_string()]);
        assert_eq!(
            extensions("image/jpeg", true),
            vec![".jpg".to_string(), ".jpe".to_string(), ".jpeg".to_string()]
        );
    }

    #[test]
    fn guess_extension_returns_first_candidate() {
        let result = call("guess_extension", &[text("text/html")]);
        assert_eq!(value::as_str(result), Some(".html"));
        let missing = call("guess_extension", &[text("application/x-missing")]);
        assert_eq!(missing, NONE);
    }

    #[test]
    fn add_type_round_trip() {
        call(
            "add_type",
            &[text("application/x-hydro-test"), text(".hydrotest")],
        );
        assert_eq!(
            guess("sample.hydrotest", true).0.as_deref(),
            Some("application/x-hydro-test")
        );
        assert_eq!(
            extensions("application/x-hydro-test", true),
            vec![".hydrotest".to_string()]
        );
        call(
            "add_type",
            &[
                text("application/x-hydro-test"),
                text(".hydrotest"),
                value::bool_value(false),
            ],
        );
        // A later strict update replaces the mapping.
        call(
            "add_type",
            &[text("application/x-hydro-test-2"), text(".hydrotest")],
        );
        assert_eq!(
            guess("sample.hydrotest", true).0.as_deref(),
            Some("application/x-hydro-test-2")
        );
    }

    #[test]
    fn constants_are_fresh_dicts_with_expected_entries() {
        let first = constant("types_map");
        let second = constant("types_map");
        assert_ne!(first, second);
        let entries = value::as_dict(first).expect("dict");
        assert!(entries.iter().any(|(key, item)| {
            value::as_str(*key) == Some(".json") && value::as_str(*item) == Some("application/json")
        }));
        let encodings = value::as_dict(constant("encodings_map")).expect("dict");
        assert!(encodings.iter().any(|(key, item)| {
            value::as_str(*key) == Some(".gz") && value::as_str(*item) == Some("gzip")
        }));
        let suffixes = value::as_dict(constant("suffix_map")).expect("dict");
        assert!(suffixes.iter().any(|(key, item)| {
            value::as_str(*key) == Some(".tgz") && value::as_str(*item) == Some(".tar.gz")
        }));
        let common = value::as_dict(constant("common_types")).expect("dict");
        assert!(common
            .iter()
            .any(|(key, _)| value::as_str(*key) == Some(".apk")));
    }
}
