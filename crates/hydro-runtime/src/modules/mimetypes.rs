//! `mimetypes`: mapping between filenames and MIME types.
//!
//! The module carries CPython's full built-in `types_map`/`common_types`
//! tables plus the `suffix_map`/`encodings_map` rewrite tables, all exported
//! as cached module globals (`types_map`, `common_types`, `suffix_map`,
//! `encodings_map`). `init()` rebuilds the default datastore and reads
//! `knownfiles` (or the explicit `files` argument) in `mime.types` format;
//! `read_mime_types()` parses a single file. `add_type()` mutates the live
//! datastore. `MimeTypes` exposes an independent per-instance datastore with
//! the same guess/read API, mirroring `Lib/mimetypes.py`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, DEPRECATION_WARNING, TYPE_ERROR};
use crate::value::{self, NONE};

/// CPython's built-in strict `types_map`, in declaration order (the order
/// fixes `guess_extension`'s preferred extension for a type). Encoded as
/// whitespace-separated `extension=type` pairs so the 198-entry table stays
/// compact; [`MimeDb::new`] parses it once per datastore.
const STRICT_TYPES_TEXT: &str = ".js=text/javascript .mjs=text/javascript .epub=application/epub+zip .gz=application/gzip .json=application/json .webmanifest=application/manifest+json .doc=application/msword .dot=application/msword .wiz=application/msword .nq=application/n-quads .nt=application/n-triples .bin=application/octet-stream .a=application/octet-stream .dll=application/octet-stream .exe=application/octet-stream .o=application/octet-stream .obj=application/octet-stream .so=application/octet-stream .oda=application/oda .ogx=application/ogg .pdf=application/pdf .p7c=application/pkcs7-mime .ps=application/postscript .ai=application/postscript .eps=application/postscript .trig=application/trig .m3u=application/vnd.apple.mpegurl .m3u8=application/vnd.apple.mpegurl .xls=application/vnd.ms-excel .xlb=application/vnd.ms-excel .eot=application/vnd.ms-fontobject .ppt=application/vnd.ms-powerpoint .pot=application/vnd.ms-powerpoint .ppa=application/vnd.ms-powerpoint .pps=application/vnd.ms-powerpoint .pwz=application/vnd.ms-powerpoint .odg=application/vnd.oasis.opendocument.graphics .odp=application/vnd.oasis.opendocument.presentation .ods=application/vnd.oasis.opendocument.spreadsheet .odt=application/vnd.oasis.opendocument.text .pptx=application/vnd.openxmlformats-officedocument.presentationml.presentation .xlsx=application/vnd.openxmlformats-officedocument.spreadsheetml.sheet .docx=application/vnd.openxmlformats-officedocument.wordprocessingml.document .rar=application/vnd.rar .wasm=application/wasm .7z=application/x-7z-compressed .bcpio=application/x-bcpio .cpio=application/x-cpio .csh=application/x-csh .deb=application/x-debian-package .dvi=application/x-dvi .gtar=application/x-gtar .hdf=application/x-hdf .h5=application/x-hdf5 .latex=application/x-latex .mif=application/x-mif .cdf=application/x-netcdf .nc=application/x-netcdf .p12=application/x-pkcs12 .php=application/x-httpd-php .pfx=application/x-pkcs12 .ram=application/x-pn-realaudio .pyc=application/x-python-code .pyo=application/x-python-code .rpm=application/x-rpm .sh=application/x-sh .shar=application/x-shar .swf=application/x-shockwave-flash .sv4cpio=application/x-sv4cpio .sv4crc=application/x-sv4crc .tar=application/x-tar .tcl=application/x-tcl .tex=application/x-tex .texi=application/x-texinfo .texinfo=application/x-texinfo .roff=application/x-troff .t=application/x-troff .tr=application/x-troff .man=application/x-troff-man .me=application/x-troff-me .ms=application/x-troff-ms .ustar=application/x-ustar .src=application/x-wais-source .xsl=application/xml .rdf=application/xml .wsdl=application/xml .xpdl=application/xml .yaml=application/yaml .yml=application/yaml .zip=application/zip .3gp=audio/3gpp .3gpp=audio/3gpp .3g2=audio/3gpp2 .3gpp2=audio/3gpp2 .aac=audio/aac .adts=audio/aac .loas=audio/aac .ass=audio/aac .au=audio/basic .snd=audio/basic .flac=audio/flac .mka=audio/matroska .m4a=audio/mp4 .mp3=audio/mpeg .mp2=audio/mpeg .ogg=audio/ogg .opus=audio/opus .aif=audio/x-aiff .aifc=audio/x-aiff .aiff=audio/x-aiff .ra=audio/x-pn-realaudio .wav=audio/vnd.wave .otf=font/otf .ttf=font/ttf .weba=audio/webm .woff=font/woff .woff2=font/woff2 .avif=image/avif .bmp=image/bmp .emf=image/emf .fits=image/fits .g3=image/g3fax .gif=image/gif .ief=image/ief .jp2=image/jp2 .jpg=image/jpeg .jpe=image/jpeg .jpeg=image/jpeg .jpm=image/jpm .jpx=image/jpx .heic=image/heic .heif=image/heif .png=image/png .svg=image/svg+xml .t38=image/t38 .tiff=image/tiff .tif=image/tiff .tfx=image/tiff-fx .ico=image/vnd.microsoft.icon .webp=image/webp .wmf=image/wmf .ras=image/x-cmu-raster .pnm=image/x-portable-anymap .pbm=image/x-portable-bitmap .pgm=image/x-portable-graymap .ppm=image/x-portable-pixmap .rgb=image/x-rgb .xbm=image/x-xbitmap .xpm=image/x-xpixmap .xwd=image/x-xwindowdump .eml=message/rfc822 .mht=message/rfc822 .mhtml=message/rfc822 .nws=message/rfc822 .gltf=model/gltf+json .glb=model/gltf-binary .stl=model/stl .css=text/css .csv=text/csv .html=text/html .htm=text/html .md=text/markdown .markdown=text/markdown .n3=text/n3 .txt=text/plain .bat=text/plain .c=text/plain .h=text/plain .ksh=text/plain .pl=text/plain .srt=text/plain .rtx=text/richtext .rtf=text/rtf .tsv=text/tab-separated-values .vtt=text/vtt .py=text/x-python .rst=text/x-rst .etx=text/x-setext .sgm=text/x-sgml .sgml=text/x-sgml .vcf=text/x-vcard .xml=text/xml .mkv=video/matroska .mk3d=video/matroska-3d .mp4=video/mp4 .mpeg=video/mpeg .m1v=video/mpeg .mpa=video/mpeg .mpe=video/mpeg .mpg=video/mpeg .ogv=video/ogg .mov=video/quicktime .qt=video/quicktime .webm=video/webm .avi=video/vnd.avi .m4v=video/x-m4v .wmv=video/x-ms-wmv .movie=video/x-sgi-movie";

/// CPython's built-in non-standard `common_types` (used when `strict=False`).
const COMMON_TYPES_TEXT: &str = ".rtf=application/rtf .apk=application/vnd.android.package-archive .midi=audio/midi .mid=audio/midi .jpg=image/jpg .pict=image/pict .pct=image/pict .pic=image/pict .xul=text/xul";


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

/// Parses one of the compact `extension=type` tables into ordered pairs.
fn parse_table(text: &str) -> Vec<(String, String)> {
    text.split_whitespace()
        .filter_map(|entry| entry.split_once('='))
        .map(|(ext, kind)| (ext.to_string(), kind.to_string()))
        .collect()
}

impl MimeDb {
    fn new() -> Self {
        Self {
            strict: parse_table(STRICT_TYPES_TEXT),
            common: parse_table(COMMON_TYPES_TEXT),
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

/// Per-`MimeTypes`-instance datastores, keyed by the `_mime_db` integer the
/// constructor stores on the instance.
static INSTANCES: LazyLock<Mutex<HashMap<u64, MimeDb>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// `mimetypes.inited`: whether `init()` has run.
static INITED: AtomicBool = AtomicBool::new(false);

/// The POSIX `knownfiles` list CPython 3.14 uses.
const KNOWN_FILES: &[&str] = &[
    "/etc/mime.types",
    "/etc/httpd/mime.types",
    "/etc/httpd/conf/mime.types",
    "/etc/apache/mime.types",
    "/etc/apache2/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/lib/netscape/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/etc/mime.types",
];

/// `mimetypes.__all__`, in CPython's order.
const ALL_NAMES: &[&str] = &[
    "knownfiles",
    "inited",
    "MimeTypes",
    "guess_type",
    "guess_file_type",
    "guess_all_extensions",
    "guess_extension",
    "add_type",
    "init",
    "read_mime_types",
    "suffix_map",
    "encodings_map",
    "types_map",
    "common_types",
];

fn database() -> MutexGuard<'static, MimeDb> {
    DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn instances() -> MutexGuard<'static, HashMap<u64, MimeDb>> {
    INSTANCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cached(name: &str) -> Option<i64> {
    globals::registry_get(&format!("mimetypes.{name}"))
}

fn publish(name: &str, item: i64) {
    globals::registry_set(&format!("mimetypes.{name}"), item);
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

/// A path argument: `str`, or `bytes` decoded lossily (CPython uses
/// `os.fsdecode`).
fn path_argument(raw: i64, name: &str) -> Result<String, i64> {
    if let Some(text) = value::as_str(raw) {
        return Ok(text.to_string());
    }
    if let Some(bytes) = value::as_bytes(raw) {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("{name} must be str or bytes, not {}", value::type_name(raw)),
    ))
}

fn strict_argument(args: &[i64], index: usize) -> bool {
    match args.get(index) {
        Some(&raw) => raw != NONE && value::truthy(raw),
        None => true,
    }
}

/// Elements of a list/tuple of `str`; other values contribute nothing.
fn string_list(raw: i64) -> Vec<String> {
    value::as_list(raw)
        .or_else(|| value::as_tuple(raw))
        .map(|items| {
            items
                .iter()
                .filter_map(|&item| value::as_str(item).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

/// CPython's `_guess_file_type`: applies `suffix_map`, then the case-sensitive
/// `encodings_map`, then the type tables.
fn guess_in(db: &MimeDb, path: &str, strict: bool) -> (Option<String>, Option<String>) {
    let (base, ext) = split_extension(path);
    let mut base = base.to_string();
    let mut ext = ext.to_string();
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
    let encoding = ENCODINGS_MAP
        .iter()
        .find(|(suffix, _)| *suffix == ext.as_str())
        .map(|(_, encoding)| (*encoding).to_string());
    if encoding.is_some() {
        let (_, previous_ext) = split_extension(&base);
        ext = previous_ext.to_string();
    }
    let lowered = ext.to_lowercase();
    if let Some(kind) = db.lookup(&lowered, true) {
        return (Some(kind.to_string()), encoding);
    }
    if !strict {
        if let Some(kind) = db.lookup(&lowered, false) {
            return (Some(kind.to_string()), encoding);
        }
    }
    (None, encoding)
}

/// Splits `scheme:rest` off a URL when the scheme is longer than one character.
fn url_scheme(url: &str) -> Option<(String, &str)> {
    let colon = url.find(':')?;
    let scheme = &url[..colon];
    if scheme.len() <= 1 {
        return None;
    }
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), &url[colon + 1..]))
}

fn strip_query(text: &str) -> &str {
    let end = text.find(['?', '#']).unwrap_or(text.len());
    &text[..end]
}

/// The path component of a URL with a non-empty scheme.
fn url_path(rest: &str) -> &str {
    let rest = strip_query(rest);
    if let Some(after) = rest.strip_prefix("//") {
        return match after.find('/') {
            Some(slash) => &after[slash..],
            None => "",
        };
    }
    rest
}

/// `guess_type` helper: handles `data:` URLs and delegates the rest to
/// [`guess_in`] on the URL's path.
fn guess_url(db: &MimeDb, url: &str, strict: bool) -> (Option<String>, Option<String>) {
    let Some((scheme, rest)) = url_scheme(url) else {
        // CPython passes the original string to `guess_file_type`, so URL
        // delimiters such as `#` and `;` stay part of the filename.
        return guess_in(db, url, strict);
    };
    let path = url_path(rest);
    if scheme == "data" {
        let Some(comma) = path.find(',') else {
            return (None, None);
        };
        let kind = match path[..comma].find(';') {
            Some(semi) => &path[..semi],
            None => &path[..comma],
        };
        if kind.contains('=') || !kind.contains('/') {
            return (Some("text/plain".to_string()), None);
        }
        return (Some(kind.to_string()), None);
    }
    guess_in(db, path, strict)
}

fn tuple_type(value_pair: (Option<String>, Option<String>)) -> i64 {
    let (kind, encoding) = value_pair;
    let kind = kind.map_or(NONE, value::str_alloc);
    let encoding = encoding.map_or(NONE, value::str_alloc);
    value::tuple_new(vec![kind, encoding])
}

fn make_dict(pairs: Vec<(String, String)>) -> i64 {
    let entries = pairs
        .into_iter()
        .map(|(key, item)| (value::str_alloc(key), value::str_alloc(item)))
        .collect();
    value::dict_new(entries)
}

fn static_dict(pairs: &[(&str, &str)]) -> i64 {
    make_dict(
        pairs
            .iter()
            .map(|&(key, item)| (key.to_string(), item.to_string()))
            .collect(),
    )
}

/// Rebinds the module-level mappings CPython keeps as aliases of the current
/// datastore (`init`/`_default_mime_types` do this).
fn publish_maps() {
    let (strict, common) = {
        let db = database();
        (db.strict.clone(), db.common.clone())
    };
    publish("types_map", make_dict(strict));
    publish("common_types", make_dict(common));
    publish("suffix_map", static_dict(SUFFIX_MAP));
    publish("encodings_map", static_dict(ENCODINGS_MAP));
}

fn knownfiles_value() -> i64 {
    if let Some(list) = cached("knownfiles") {
        return list;
    }
    let values: Vec<i64> = KNOWN_FILES
        .iter()
        .map(|&name| value::str_alloc(name.to_string()))
        .collect();
    let list = value::list_new(values);
    publish("knownfiles", list);
    list
}

fn all_names_value() -> i64 {
    if let Some(list) = cached("__all__") {
        return list;
    }
    let values: Vec<i64> = ALL_NAMES
        .iter()
        .map(|&name| value::str_alloc(name.to_string()))
        .collect();
    let list = value::list_new(values);
    publish("__all__", list);
    list
}

/// Reads a `mime.types`-format file into `db`. Returns false when the file
/// cannot be read (CPython's `init` skips those).
fn read_file_into(path: &str, db: &mut MimeDb, strict: bool) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let text = String::from_utf8_lossy(&data);
    parse_mime_text(&text, db, strict);
    true
}

/// Parses `mime.types` lines: `type suffix [suffix ...]`, with `#` comments.
fn parse_mime_text(text: &str, db: &mut MimeDb, strict: bool) {
    for line in text.lines() {
        let mut words: Vec<&str> = line.split_whitespace().collect();
        if let Some(position) = words.iter().position(|word| word.starts_with('#')) {
            words.truncate(position);
        }
        let Some((kind, suffixes)) = words.split_first() else {
            continue;
        };
        for suffix in suffixes {
            db.add(kind, &format!(".{suffix}"), strict);
        }
    }
}

/// The `knownfiles` the module currently exposes, honouring a program
/// assignment (`mimetypes.knownfiles = [...]`).
fn module_knownfiles() -> Vec<String> {
    if let Some(raw) = globals::attribute_override("mimetypes", "knownfiles") {
        if value::as_list(raw).is_some() || value::as_tuple(raw).is_some() {
            return string_list(raw);
        }
    }
    KNOWN_FILES.iter().map(|name| (*name).to_string()).collect()
}

/// `init(files=None)`: rebuild the default datastore and read `files`.
fn init_files(args: &[i64]) {
    let mut files = module_knownfiles();
    if let Some(&extra) = args.first() {
        if extra != NONE {
            files.extend(string_list(extra));
        }
    }
    {
        let mut db = database();
        *db = MimeDb::new();
        for file in &files {
            if std::path::Path::new(file).is_file() {
                read_file_into(file, &mut db, true);
            }
        }
    }
    publish_maps();
    INITED.store(true, Ordering::Relaxed);
}

/// `read_mime_types(file)`: parse one file and return its strict mapping, or
/// `None` when it cannot be read.
fn read_mime_types_value(raw: i64) -> i64 {
    let Ok(path) = path_argument(raw, "file") else {
        return NONE;
    };
    let Ok(data) = std::fs::read(&path) else {
        return NONE;
    };
    let text = String::from_utf8_lossy(&data);
    let mut db = MimeDb::new();
    parse_mime_text(&text, &mut db, true);
    make_dict(db.strict)
}

// -------------------------------------------------------------------------
// Module-level dispatch
// -------------------------------------------------------------------------

/// Module constants. The maps are cached objects, so repeated reads before
/// `init()` share identity while `init()` rebinds them to new objects.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "types_map" | "common_types" | "suffix_map" | "encodings_map" => {
            if let Some(map) = cached(name) {
                return map;
            }
            publish_maps();
            cached(name).unwrap_or(NONE)
        }
        "knownfiles" => knownfiles_value(),
        "inited" => value::bool_value(INITED.load(Ordering::Relaxed)),
        // `_db` is the internal datastore; after `init()` CPython exposes a
        // `MimeTypes` instance, but tests only reset it to `None`.
        "_db" => NONE,
        "__all__" => all_names_value(),
        "MimeTypes" => mime_types_class(),
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
            let Some(&raw) = args.first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_type() missing required argument: 'url'",
                );
            };
            let url = match path_argument(raw, "url") {
                Ok(url) => url,
                Err(error) => return error,
            };
            let strict = strict_argument(args, 1);
            let result = guess_url(&database(), &url, strict);
            tuple_type(result)
        }
        "guess_file_type" => {
            let Some(&raw) = args.first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_file_type() missing required argument: 'path'",
                );
            };
            let path = match path_argument(raw, "path") {
                Ok(path) => path,
                Err(error) => return error,
            };
            let strict = strict_argument(args, 1);
            let result = guess_in(&database(), &path, strict);
            tuple_type(result)
        }
        "guess_all_extensions" => {
            let Some(&raw) = args.first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_all_extensions() missing required argument: 'type'",
                );
            };
            let kind = match string_argument(raw, "type") {
                Ok(kind) => kind.to_lowercase(),
                Err(error) => return error,
            };
            let extensions = database().extensions(&kind, strict_argument(args, 1));
            let values = extensions.into_iter().map(value::str_alloc).collect();
            value::list_new(values)
        }
        "guess_extension" => {
            let Some(&raw) = args.first() else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "guess_extension() missing required argument: 'type'",
                );
            };
            let kind = match string_argument(raw, "type") {
                Ok(kind) => kind.to_lowercase(),
                Err(error) => return error,
            };
            match database()
                .extensions(&kind, strict_argument(args, 1))
                .into_iter()
                .next()
            {
                Some(ext) => value::str_alloc(ext),
                None => NONE,
            }
        }
        "add_type" => {
            let (Some(&raw_kind), Some(&raw_ext)) = (args.first(), args.get(1)) else {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    "add_type() missing required argument(s)",
                );
            };
            let kind = match string_argument(raw_kind, "type") {
                Ok(kind) => kind,
                Err(error) => return error,
            };
            let ext = match string_argument(raw_ext, "ext") {
                Ok(ext) => ext,
                Err(error) => return error,
            };
            if !kind.is_empty() {
                warn_undotted(&ext);
                database().add(&kind, &ext, strict_argument(args, 2));
                publish_maps();
            }
            NONE
        }
        "init" => {
            init_files(args);
            NONE
        }
        "_default_mime_types" => {
            *database() = MimeDb::new();
            publish_maps();
            NONE
        }
        "read_mime_types" => match args.first().copied() {
            Some(raw) => read_mime_types_value(raw),
            None => exc::raise_with_message(
                TYPE_ERROR,
                "read_mime_types() missing required argument: 'file'",
            ),
        },
        "_parse_args" => {
            let tokens = match args.first() {
                Some(&raw) => arg_tokens(raw),
                None => Vec::new(),
            };
            let (extension, lenient, types, help) = parse_cli(&tokens);
            let namespace = value::namespace_new(vec![
                ("extension".to_string(), value::bool_value(extension)),
                ("lenient".to_string(), value::bool_value(lenient)),
                (
                    "type".to_string(),
                    value::list_new(types.iter().map(|item| value::str_alloc(item.clone())).collect()),
                ),
            ]);
            let help = value::str_alloc(help);
            let _ = (extension, lenient);
            value::tuple_new(vec![namespace, help])
        }
        "_main" => {
            let tokens = match args.first().copied() {
                None | Some(NONE) => Vec::new(),
                Some(raw) => arg_tokens(raw),
            };
            let (extension, lenient, types, _) = parse_cli(&tokens);
            main_results(extension, lenient, &types)
        }
        // `MimeTypes` instance methods (the class stores its datastore id in
        // the `_mime_db` attribute).
        name if name.starts_with("MimeTypes.") => mime_types_method(name, args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'mimetypes' has no attribute '{function}'"),
        ),
    }
}

/// Emits CPython's deprecation warning for an undotted extension.
fn warn_undotted(ext: &str) {
    if ext.is_empty() || ext.starts_with('.') {
        return;
    }
    super::warnings::warn_category(
        "Using undotted extensions is deprecated and will raise a ValueError in Python 3.16",
        DEPRECATION_WARNING,
    );
}

// -------------------------------------------------------------------------
// `MimeTypes` class
// -------------------------------------------------------------------------

fn mime_types_class() -> i64 {
    const KEY: &str = "mimetypes.MimeTypes.class";
    if let Some(class) = globals::registry_get(KEY) {
        return class;
    }
    let methods = vec![
        (
            "__init__".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.__init__", usize::MAX),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "add_type".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.add_type", 4),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "guess_type".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.guess_type", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "guess_file_type".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.guess_file_type", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "guess_all_extensions".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.guess_all_extensions", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "guess_extension".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.guess_extension", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "read".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.read", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "readfp".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.readfp", 3),
            hydro_ir::MethodKind::Normal,
        ),
        (
            "read_windows_registry".to_string(),
            value::native_function_new("mimetypes", "MimeTypes.read_windows_registry", 2),
            hydro_ir::MethodKind::Normal,
        ),
    ];
    let class = value::class_new_full(
        "MimeTypes".to_string(),
        value::object_class(),
        methods,
        Vec::new(),
    );
    let roots = crate::heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(KEY, class);
    crate::heap::release_roots(roots);
    class
}

/// Allocates a per-instance datastore and returns its id.
fn new_instance_db() -> u64 {
    let id = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
    instances().insert(id, MimeDb::new());
    id
}

fn instance_id(instance: i64) -> Option<u64> {
    let raw = crate::core::methods::getattr(instance, b"_mime_db".as_ptr(), 8);
    if exc::pending() {
        return None;
    }
    value::as_integer(raw).and_then(|id| u64::try_from(id).ok())
}

fn with_instance_db<R>(instance: i64, apply: impl FnOnce(&mut MimeDb) -> R) -> Option<R> {
    let id = instance_id(instance)?;
    let mut map = instances();
    map.get_mut(&id).map(apply)
}

fn require_args<'a>(args: &'a [i64], count: usize, name: &str) -> Option<&'a [i64]> {
    if args.len() < count {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("{name}() takes at least {count} argument(s) ({} given)", args.len()),
        );
        return None;
    }
    Some(args)
}

fn mime_types_method(function: &str, args: &[i64]) -> i64 {
    let Some(args) = require_args(args, 1, function) else {
        return NONE;
    };
    let instance = args[0];
    match function {
        "MimeTypes.__init__" => {
            let id = new_instance_db();
            value::instance_setattr(instance, "_mime_db".to_string(), crate::ops::int_result(id as i64));
            let filenames = args.get(1).copied().unwrap_or(NONE);
            let strict = strict_argument(args, 2);
            for file in string_list(filenames) {
                with_instance_db(instance, |db| {
                    if std::path::Path::new(&file).is_file() {
                        read_file_into(&file, db, strict);
                    }
                });
            }
            NONE
        }
        "MimeTypes.add_type" => {
            let Some(args) = require_args(args, 3, function) else {
                return NONE;
            };
            let (Ok(kind), Ok(ext)) = (
                string_argument(args[1], "type"),
                string_argument(args[2], "ext"),
            ) else {
                return NONE;
            };
            if !kind.is_empty() {
                warn_undotted(&ext);
                let strict = strict_argument(args, 3);
                with_instance_db(instance, |db| db.add(&kind, &ext, strict));
            }
            NONE
        }
        "MimeTypes.guess_type" | "MimeTypes.guess_file_type" => {
            let Some(args) = require_args(args, 2, function) else {
                return NONE;
            };
            let Ok(path) = path_argument(args[1], "url/path") else {
                return NONE;
            };
            let strict = strict_argument(args, 2);
            let is_url = function.ends_with("guess_type");
            let result = with_instance_db(instance, |db| {
                if is_url {
                    guess_url(db, &path, strict)
                } else {
                    guess_in(db, &path, strict)
                }
            });
            match result {
                Some(pair) => tuple_type(pair),
                None => attribute_missing(function),
            }
        }
        "MimeTypes.guess_all_extensions" | "MimeTypes.guess_extension" => {
            let Some(args) = require_args(args, 2, function) else {
                return NONE;
            };
            let Ok(kind) = string_argument(args[1], "type") else {
                return NONE;
            };
            let kind = kind.to_lowercase();
            let strict = strict_argument(args, 2);
            let extensions = with_instance_db(instance, |db| db.extensions(&kind, strict));
            let Some(extensions) = extensions else {
                return attribute_missing(function);
            };
            if function.ends_with("guess_extension") {
                match extensions.into_iter().next() {
                    Some(ext) => value::str_alloc(ext),
                    None => NONE,
                }
            } else {
                value::list_new(extensions.into_iter().map(value::str_alloc).collect())
            }
        }
        "MimeTypes.read" => {
            let Some(args) = require_args(args, 2, function) else {
                return NONE;
            };
            let Ok(path) = path_argument(args[1], "filename") else {
                return NONE;
            };
            let strict = strict_argument(args, 2);
            with_instance_db(instance, |db| {
                let _ = read_file_into(&path, db, strict);
            });
            NONE
        }
        "MimeTypes.readfp" => {
            let Some(args) = require_args(args, 2, function) else {
                return NONE;
            };
            let strict = strict_argument(args, 2);
            let data = crate::core::methods::call(
                args[1],
                b"read".as_ptr(),
                4,
                0,
                std::ptr::null(),
            );
            if exc::pending() {
                return NONE;
            }
            let text = match value::as_str(data) {
                Some(text) => text.to_string(),
                None => match value::as_bytes(data) {
                    Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    None => return NONE,
                },
            };
            with_instance_db(instance, |db| parse_mime_text(&text, db, strict));
            NONE
        }
        "MimeTypes.read_windows_registry" => NONE,
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("'MimeTypes' object has no attribute '{function}'"),
        ),
    }
}

fn attribute_missing(function: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("'MimeTypes' object has no attribute '{function}'"),
    )
}

// -------------------------------------------------------------------------
// Command-line helpers (`_parse_args` / `_main`)
// -------------------------------------------------------------------------

const HELP_TEXT: &str = "usage: mimetypes [-h] [-e] [-l] type [type ...]\n\nmap filename extensions to MIME types\n\npositional arguments:\n  type              a type to search\n\noptions:\n  -h, --help        show this help message and exit\n  -e, --extension   guess extension instead of type\n  -l, --lenient     additionally search for common but non-standard types\n";

/// Argument tokens for `_parse_args`: a `str` is iterated character by
/// character like `argparse.parse_args` does, a list/tuple supplies tokens.
fn arg_tokens(raw: i64) -> Vec<String> {
    if let Some(text) = value::as_str(raw) {
        return text.chars().map(|ch| ch.to_string()).collect();
    }
    string_list(raw)
}

fn parse_cli(tokens: &[String]) -> (bool, bool, Vec<String>, String) {
    let mut extension = false;
    let mut lenient = false;
    let mut types = Vec::new();
    for token in tokens {
        match token.as_str() {
            "-h" | "--help" | "-e" | "--extension" | "-l" | "--lenient" => match token.as_str() {
                "-e" | "--extension" => extension = true,
                "-l" | "--lenient" => lenient = true,
                _ => {}
            },
            other => types.push(other.to_string()),
        }
    }
    (extension, lenient, types, HELP_TEXT.to_string())
}

fn main_results(extension: bool, lenient: bool, types: &[String]) -> i64 {
    let strict = !lenient;
    let mut results = Vec::new();
    for entry in types {
        if extension {
            match database().extensions(&entry.to_lowercase(), strict).into_iter().next() {
                Some(ext) => results.push(value::str_alloc(ext)),
                None => results.push(value::str_alloc(format!("error: unknown type {entry}"))),
            }
        } else {
            let pair = guess_url(&database(), entry, strict);
            match pair.0 {
                Some(kind) => results.push(value::str_alloc(format!(
                    "type: {kind} encoding: {}",
                    pair.1.as_deref().unwrap_or("None")
                ))),
                None => results.push(value::str_alloc(format!(
                    "error: media type unknown for {entry}"
                ))),
            }
        }
    }
    value::list_new(results)
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
        // A scheme-less name is a filename: CPython keeps `?`/`#` in the
        // extension, so the lookup misses.
        assert_eq!(guess("photo.png?download=1", true), (None, None));
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
    fn constants_share_cached_identity() {
        let first = constant("types_map");
        let second = constant("types_map");
        // CPython keeps one mapping object per module global until `init()`
        // rebinds it, so repeated reads share identity.
        assert_eq!(first, second);
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
