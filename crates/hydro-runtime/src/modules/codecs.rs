//! `_codecs`: the built-in codecs and the codec/error-handler registries.
//!
//! Hydro's strings are UTF-8, and the runtime implements three codecs
//! natively: `utf-8`, `ascii` and `latin-1` (ISO-8859-1). Latin-1 maps every
//! byte to the code point of the same value and therefore never fails to
//! decode; ASCII rejects bytes above `0x7f` and both single-byte codecs
//! reject code points they cannot represent. Every other encoding name fails
//! with `LookupError` before any conversion happens; it is never silently
//! treated as UTF-8.
//!
//! The codec registry keeps callable search functions registered with
//! `register()` and consults them from `lookup()` after the built-in names
//! (and their aliases) are answered. Registered search functions must return
//! a 4-tuple (encode, decode, incremental encoder, incremental decoder), like
//! CPython's `CodecInfo` tuple. The built-in entries are the stateless
//! `utf_8_*`/`ascii_*`/`latin_1_*` functions; `encode`/`decode` themselves
//! take the encoding name and route to the same engines.
//!
//! The error-handler registry stores callables. `strict`, `ignore`,
//! `replace` and `surrogateescape` are built in (and can be overridden by
//! `register_error`, as in CPython). During decoding, a registered handler is
//! called with a `UnicodeDecodeError` object and must return
//! `(replacement_str, new_pos)`. Hydro's exception objects expose `args`
//! rather than `.object/.start/.end`, so the error object carries
//! `(encoding, object_repr, start, end, reason)` as its args tuple. Encoding
//! handlers accept either a `str` (encoded with the target codec, the shape
//! the runtime's own handlers use) or a `bytes` replacement, which is
//! appended verbatim as CPython's encoder protocol specifies.
//!
//! Lone surrogates are real Python code points in this runtime: a string
//! payload keeps `U+D800..U+DFFF` in a reserved private-use mirror block, and
//! `value::str_codes`/`value::str_from_codes` translate at the boundary.
//! Every codec path here uses those accessors, so `surrogateescape` maps an
//! undecodable byte `b` to code point `0xDC00 + b` (and back), `surrogatepass`
//! emits and accepts the generalized UTF-8 `ED A0..BF ..` forms, and `strict`
//! reports `UnicodeDecodeError`/`UnicodeEncodeError` with the offending span.
//!
//! `escape_encode`/`escape_decode` implement CPython's `unicode_escape`
//! subset used by the `codecs` module.

use std::cell::RefCell;

use crate::builtins as core_builtins;
use crate::core::globals;
use crate::exc::{
    self, LOOKUP_ERROR, RUNTIME_ERROR, TYPE_ERROR, UNICODE_DECODE_ERROR, UNICODE_ENCODE_ERROR,
    UNICODE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

thread_local! {
    /// Codec search functions, in registration order.
    static SEARCH_FUNCTIONS: RefCell<Vec<i64>> = const { RefCell::new(Vec::new()) };
    /// Named error handlers; later registrations replace earlier ones.
    static ERROR_HANDLERS: RefCell<Vec<(String, i64)>> = const { RefCell::new(Vec::new()) };
}

/// How malformed input is treated.
enum ErrorMode {
    Strict,
    Ignore,
    Replace,
    /// PEP 383: an undecodable byte `b` becomes code point `0xDC00 + b`, and
    /// that code point encodes back to the byte.
    SurrogateEscape,
    /// Lone surrogates pass through UTF-8 (and are accepted when decoding).
    SurrogatePass,
    /// `\xNN`, `\uNNNN` and `\UNNNNNNNN` escapes.
    BackslashReplace,
    /// `\N{NAME}` for encoding, falling back to `\uNNNN`.
    NameReplace,
    /// `&#NNN;` for encoding.
    XmlCharrefReplace,
    Custom(i64),
}

/// The codecs the runtime implements natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    Utf8,
    Ascii,
    Latin1,
    Utf8Sig,
    Utf16,
    Utf16Le,
    Utf16Be,
    Utf32,
    Utf32Le,
    Utf32Be,
    Utf7,
    UnicodeEscape,
    RawUnicodeEscape,
    Charmap,
    Rot13,
    Undefined,
    /// A single-byte (or otherwise `iconv`-convertible) codec: the
    /// `_codecs` programmatic name, the canonical `CodecInfo.name` and the
    /// `iconv` charset spelling.
    Charset {
        prog: &'static str,
        name: &'static str,
        charset: &'static str,
    },
    /// A codec from [`super::codecs_native`] (CJK and ISO-2022 families).
    Multibyte {
        prog: &'static str,
    },
}

impl Codec {
    /// The spelling CPython uses in error messages and registry keys.
    fn name(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf-8",
            Codec::Ascii => "ascii",
            Codec::Latin1 => "latin-1",
            Codec::Utf8Sig => "utf-8-sig",
            Codec::Utf16 => "utf-16",
            Codec::Utf16Le => "utf-16-le",
            Codec::Utf16Be => "utf-16-be",
            Codec::Utf32 => "utf-32",
            Codec::Utf32Le => "utf-32-le",
            Codec::Utf32Be => "utf-32-be",
            Codec::Utf7 => "utf-7",
            Codec::UnicodeEscape => "unicode-escape",
            Codec::RawUnicodeEscape => "raw-unicode-escape",
            Codec::Charmap => "charmap",
            Codec::Rot13 => "rot-13",
            Codec::Undefined => "undefined",
            Codec::Charset { name, .. } => name,
            Codec::Multibyte { prog } => prog,
        }
    }

    /// The programmatic name `_codecs` uses for the codec's entry points
    /// (`utf_8_encode`, `gb2312_decode`, ...).
    fn prog(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf_8",
            Codec::Ascii => "ascii",
            Codec::Latin1 => "latin_1",
            Codec::Utf8Sig => "utf_8_sig",
            Codec::Utf16 => "utf_16",
            Codec::Utf16Le => "utf_16_le",
            Codec::Utf16Be => "utf_16_be",
            Codec::Utf32 => "utf_32",
            Codec::Utf32Le => "utf_32_le",
            Codec::Utf32Be => "utf_32_be",
            Codec::Utf7 => "utf_7",
            Codec::UnicodeEscape => "unicode_escape",
            Codec::RawUnicodeEscape => "raw_unicode_escape",
            Codec::Charmap => "charmap",
            Codec::Rot13 => "rot_13",
            Codec::Undefined => "undefined",
            Codec::Charset { prog, .. } => prog,
            Codec::Multibyte { prog } => prog,
        }
    }

    /// Whether the codec converts text to bytes (`True`) or is a non-text
    /// codec such as `rot-13` (`False`, the `_is_text_encoding` flag).
    fn is_text(self) -> bool {
        !matches!(self, Codec::Rot13)
    }

    /// Highest code point the codec can represent as one unit.
    fn limit(self) -> u32 {
        match self {
            Codec::Utf8
            | Codec::Utf8Sig
            | Codec::Utf16
            | Codec::Utf16Le
            | Codec::Utf16Be
            | Codec::Utf32
            | Codec::Utf32Le
            | Codec::Utf32Be
            | Codec::Utf7
            | Codec::UnicodeEscape
            | Codec::RawUnicodeEscape
            | Codec::Multibyte { .. } => 0x10_FFFF,
            Codec::Ascii => 0x7f,
            Codec::Latin1 => 0xff,
            // `charmap` with no map accepts every code point below 0x100 on
            // encode and rejects the rest; the generic engine reports that.
            Codec::Charmap => 0xff,
            Codec::Rot13 | Codec::Undefined | Codec::Charset { .. } => 0x10_FFFF,
        }
    }
}

/// Single-byte codecs served through the system `iconv`.
///
/// Each entry is `(programmatic name, canonical CodecInfo.name, iconv
/// charset)`. `codecs.lookup` answers them only when the system `iconv`
/// actually provides the charset; a missing table is reported as
/// `LookupError` rather than silently treated as Latin-1.
const CHARSET_CODECS: &[(&str, &str, &str)] = &[
    ("cp037", "cp037", "IBM037"),
    ("cp1006", "cp1006", "IBM1006"),
    ("cp1026", "cp1026", "IBM1026"),
    ("cp1125", "cp1125", "CP1125"),
    ("cp1140", "cp1140", "IBM1140"),
    ("cp1250", "cp1250", "CP1250"),
    ("cp1251", "cp1251", "CP1251"),
    ("cp1252", "cp1252", "CP1252"),
    ("cp1253", "cp1253", "CP1253"),
    ("cp1254", "cp1254", "CP1254"),
    ("cp1255", "cp1255", "CP1255"),
    ("cp1256", "cp1256", "CP1256"),
    ("cp1257", "cp1257", "CP1257"),
    ("cp1258", "cp1258", "CP1258"),
    ("cp273", "cp273", "IBM273"),
    ("cp424", "cp424", "IBM424"),
    ("cp437", "cp437", "CP437"),
    ("cp500", "cp500", "IBM500"),
    ("cp720", "cp720", "CP720"),
    ("cp737", "cp737", "CP737"),
    ("cp775", "cp775", "CP775"),
    ("cp850", "cp850", "CP850"),
    ("cp852", "cp852", "CP852"),
    ("cp855", "cp855", "CP855"),
    ("cp856", "cp856", "CP856"),
    ("cp857", "cp857", "CP857"),
    ("cp858", "cp858", "CP858"),
    ("cp860", "cp860", "CP860"),
    ("cp861", "cp861", "CP861"),
    ("cp862", "cp862", "CP862"),
    ("cp863", "cp863", "CP863"),
    ("cp864", "cp864", "CP864"),
    ("cp865", "cp865", "CP865"),
    ("cp866", "cp866", "CP866"),
    ("cp869", "cp869", "CP869"),
    ("cp874", "cp874", "CP874"),
    ("cp875", "cp875", "IBM875"),
    ("hp_roman8", "hp-roman8", "HP-ROMAN8"),
    ("iso8859_2", "iso8859-2", "ISO-8859-2"),
    ("iso8859_3", "iso8859-3", "ISO-8859-3"),
    ("iso8859_4", "iso8859-4", "ISO-8859-4"),
    ("iso8859_5", "iso8859-5", "ISO-8859-5"),
    ("iso8859_6", "iso8859-6", "ISO-8859-6"),
    ("iso8859_7", "iso8859-7", "ISO-8859-7"),
    ("iso8859_8", "iso8859-8", "ISO-8859-8"),
    ("iso8859_9", "iso8859-9", "ISO-8859-9"),
    ("iso8859_10", "iso8859-10", "ISO-8859-10"),
    ("iso8859_11", "iso8859-11", "ISO-8859-11"),
    ("iso8859_13", "iso8859-13", "ISO-8859-13"),
    ("iso8859_14", "iso8859-14", "ISO-8859-14"),
    ("iso8859_15", "iso8859-15", "ISO-8859-15"),
    ("iso8859_16", "iso8859-16", "ISO-8859-16"),
    ("koi8_r", "koi8-r", "KOI8-R"),
    ("koi8_t", "koi8-t", "KOI8-T"),
    ("koi8_u", "koi8-u", "KOI8-U"),
    ("kz1048", "kz1048", "RK1048"),
    ("mac_cyrillic", "mac-cyrillic", "MAC-CYRILLIC"),
    ("mac_greek", "mac-greek", "MAC-GREEK"),
    ("mac_iceland", "mac-iceland", "MAC-ICELAND"),
    ("mac_latin2", "mac-latin2", "MAC-CENTRALEUROPE"),
    ("mac_roman", "mac-roman", "MACINTOSH"),
    ("mac_turkish", "mac-turkish", "MAC-TURKISH"),
    ("palmos", "palmos", "PALMOS"),
    ("ptcp154", "ptcp154", "PT154"),
    ("tis_620", "tis-620", "TIS-620"),
];

/// CJK codecs implemented by [`super::codecs_native`].
const MULTIBYTE_CODECS: &[&str] = &[
    "big5",
    "big5hkscs",
    "cp932",
    "cp949",
    "cp950",
    "euc_jis_2004",
    "euc_jisx0213",
    "euc_jp",
    "euc_kr",
    "gb18030",
    "gb2312",
    "gbk",
    "hz",
    "iso2022_jp",
    "iso2022_jp_1",
    "iso2022_jp_2",
    "iso2022_jp_2004",
    "iso2022_jp_3",
    "iso2022_jp_ext",
    "iso2022_kr",
    "johab",
    "shift_jis",
    "shift_jis_2004",
    "shift_jisx0213",
];

/// CPython's encoding-name normalization: ASCII alphanumerics are lowercased,
/// `.` is kept, and every other run of characters becomes a single `_`
/// (non-ASCII characters contribute nothing).
fn normalize_encoding(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut punct = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '.' {
            if punct && !out.is_empty() {
                out.push('_');
            }
            if character.is_ascii() {
                out.push(character.to_ascii_lowercase());
            }
            punct = false;
        } else {
            punct = true;
        }
    }
    out
}

/// Resolves an encoding name, including the CPython aliases, to a codec the
/// runtime implements itself. Names are normalized the way `_codecs.lookup`
/// does before consulting the search-function registry.
fn codec_for_name(name: &str) -> Option<Codec> {
    if name.contains('\0') {
        return None;
    }
    let normalized = normalize_encoding(name);
    if let Some(codec) = fixed_codec(&normalized) {
        return Some(codec);
    }
    if let Some(entry) = CHARSET_CODECS.iter().find(|entry| entry.0 == normalized) {
        return Some(Codec::Charset {
            prog: entry.0,
            name: entry.1,
            charset: entry.2,
        });
    }
    if let Some(prog) = MULTIBYTE_CODECS.iter().find(|prog| **prog == normalized) {
        return Some(Codec::Multibyte { prog });
    }
    // The `iso_8859_N` alias of the `iso8859_N` table entries.
    if let Some(rest) = normalized.strip_prefix("iso_8859_") {
        let wanted = format!("iso8859_{rest}");
        if let Some(entry) = CHARSET_CODECS.iter().find(|entry| entry.0 == wanted) {
            return Some(Codec::Charset {
                prog: entry.0,
                name: entry.1,
                charset: entry.2,
            });
        }
    }
    None
}

/// The codecs with fixed names, including the CPython aliases.
fn fixed_codec(normalized: &str) -> Option<Codec> {
    match normalized {
        "utf_8" | "utf8" | "u8" | "cp65001" => Some(Codec::Utf8),
        "utf_8_sig" | "utf8_sig" => Some(Codec::Utf8Sig),
        "ascii" | "us_ascii" | "us" | "646" | "ansi_x3.4.1968" => Some(Codec::Ascii),
        "latin_1" | "latin1" | "latin" | "iso8859_1" | "iso_8859_1" | "iso_latin_1" | "l1"
        | "cp819" | "8859" | "819" | "iso8859" | "cp28591" => Some(Codec::Latin1),
        "utf_16" | "utf16" | "u16" | "unicodelittleunmarked" => Some(Codec::Utf16),
        "utf_16_le" | "utf16_le" | "utf_16le" | "utf16le" | "unicodelittle" => Some(Codec::Utf16Le),
        "utf_16_be" | "utf16_be" | "utf_16be" | "utf16be" | "unicodebig" | "unicodebigunmarked" => {
            Some(Codec::Utf16Be)
        }
        "utf_32" | "utf32" | "u32" => Some(Codec::Utf32),
        "utf_32_le" | "utf32_le" | "utf_32le" | "utf32le" => Some(Codec::Utf32Le),
        "utf_32_be" | "utf32_be" | "utf_32be" | "utf32be" => Some(Codec::Utf32Be),
        "utf_7" | "utf7" | "u7" => Some(Codec::Utf7),
        "unicode_escape" | "unicodeescape" => Some(Codec::UnicodeEscape),
        "raw_unicode_escape" | "rawunicodeescape" => Some(Codec::RawUnicodeEscape),
        "charmap" => Some(Codec::Charmap),
        "rot_13" | "rot13" => Some(Codec::Rot13),
        "undefined" => Some(Codec::Undefined),
        _ => None,
    }
}

pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "encode" => encode(args),
        "decode" => decode(args),
        "utf_8_encode" => utf_8_encode(args),
        "utf_8_decode" => utf_8_decode(args),
        "ascii_encode" => ascii_encode(args),
        "ascii_decode" => ascii_decode(args),
        "latin_1_encode" => latin_1_encode(args),
        "latin_1_decode" => latin_1_decode(args),
        "escape_encode" => escape_encode(args),
        "escape_decode" => escape_decode(args),
        "lookup" => lookup(args),
        "getdecoder" => codec_member(args, "decode"),
        "getencoder" => codec_member(args, "encode"),
        // The native codecs are stateless for single-call encode/decode, so
        // the incremental entry points hand back the same callables. That is
        // enough for `_pyio`'s module-level imports; stateful streaming is
        // still future work.
        "getincrementaldecoder" => codec_member(args, "decode"),
        "getincrementalencoder" => codec_member(args, "encode"),
        "register" => register(args),
        "unregister" => unregister(args),
        "register_error" => register_error(args),
        "_unregister_error" => unregister_error(args),
        "lookup_error" => lookup_error(args),
        "strict_errors" => strict_errors(args),
        "ignore_errors" => ignore_errors(args),
        "replace_errors" => replace_errors(args),
        "surrogateescape_errors" => surrogateescape_errors(args),
        // The stateless entry points [`codec_info`] advertises for the codecs
        // without a hand-written function above: `utf_16_encode`,
        // `cp1252_decode`, `shift_jis_encode`, ...
        _ => codec_entry(function, args),
    }
}

/// Dispatches a generic `{codec}_encode`/`{codec}_decode` entry point.
///
/// The suffix is stripped and the remainder resolved through the same alias
/// table `lookup` uses, so `utf_16_le_decode` and `iso8859_15_encode` reach the
/// shared engines instead of failing as unknown module attributes.
fn codec_entry(function: &str, args: &[i64]) -> i64 {
    let (prog, encode) = match function
        .strip_suffix("_encode")
        .map(|prog| (prog, true))
        .or_else(|| function.strip_suffix("_decode").map(|prog| (prog, false)))
    {
        Some(entry) => entry,
        None => return unknown_entry(function),
    };
    let Some(codec) = codec_for_name(prog) else {
        return unknown_entry(function);
    };
    if encode {
        encode_entry(args, codec, function)
    } else {
        decode_entry(args, codec, function)
    }
}

/// `AttributeError` for a name `_codecs` does not expose.
fn unknown_entry(function: &str) -> i64 {
    exc::raise_with_message(
        exc::ATTRIBUTE_ERROR,
        &format!("module '_codecs' has no attribute '{function}'"),
    )
}

// Argument helpers

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn text_argument(function: &str, slot: &str, value: i64) -> Result<&'static str, i64> {
    match value::as_str(value) {
        Some(text) => Ok(text),
        None => Err(type_error(&format!(
            "{function}() argument '{slot}' must be str, not {}",
            value::type_name(value)
        ))),
    }
}

fn bytes_argument(_function: &str, value: i64) -> Result<&'static [u8], i64> {
    match value::as_bytes(value) {
        Some(data) => Ok(data),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "a bytes-like object is required, not '{}'",
                value::type_name(value)
            ),
        )),
    }
}

/// Resolves the optional `encoding` argument (index 1) to a built-in codec.
fn encoding_argument(args: &[i64], function: &str) -> Result<Codec, i64> {
    let raw = match args.get(1).copied() {
        None => "utf-8",
        Some(value) => text_argument(function, "encoding", value)?,
    };
    if raw.contains('\0') {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "embedded null character",
        ));
    }
    codec_for_name(raw).ok_or_else(|| lookup_error_message(&format!("unknown encoding: {raw}")))
}

/// Resolves the optional `errors` argument (index 2). `encode`/`decode`
/// require a string (CPython rejects `None` there).
fn errors_argument(args: &[i64], function: &str) -> Result<&'static str, i64> {
    match args.get(2).copied() {
        None => Ok("strict"),
        Some(value) => text_argument(function, "errors", value),
    }
}

/// Resolves the `errors` argument of the stateless per-codec entry points,
/// which accept `None` as "strict" like CPython's `ascii_decode` family.
fn optional_errors_argument(
    args: &[i64],
    index: usize,
    function: &str,
) -> Result<&'static str, i64> {
    match args.get(index).copied() {
        None | Some(NONE) => Ok("strict"),
        Some(value) => match value::as_str(value) {
            Some(errors) => Ok(errors),
            None => Err(type_error(&format!(
                "{function}() argument {} must be str or None, not {}",
                index + 1,
                value::type_name(value)
            ))),
        },
    }
}

/// Resolves an error-handler name to its behavior, consulting the registry
/// first so a registered handler can override a built-in name.
fn error_mode(name: &str) -> Result<ErrorMode, i64> {
    if let Some(handler) = registered_error(name) {
        return Ok(ErrorMode::Custom(handler));
    }
    match name {
        "strict" => Ok(ErrorMode::Strict),
        "ignore" => Ok(ErrorMode::Ignore),
        "replace" => Ok(ErrorMode::Replace),
        "surrogateescape" => Ok(ErrorMode::SurrogateEscape),
        _ => Err(lookup_error_message(&format!(
            "unknown error handler name '{name}'"
        ))),
    }
}

fn lookup_error_message(message: &str) -> i64 {
    exc::raise_with_message(LOOKUP_ERROR, message)
}

fn registered_error(name: &str) -> Option<i64> {
    ERROR_HANDLERS.with(|handlers| {
        handlers
            .borrow()
            .iter()
            .rev()
            .find(|(registered, _)| registered.as_str() == name)
            .map(|(_, handler)| *handler)
    })
}

// encode / decode

/// `codecs.encode(obj, encoding='utf-8', errors='strict')`.
///
/// Resolution goes through [`lookup`], so a registered search function (the
/// `encodings` package or a test codec) contributes its own encoder with the
/// same semantics `str.encode` has in CPython.
fn encode(args: &[i64]) -> i64 {
    codec_call(args, "encode")
}

/// `codecs.decode(obj, encoding='utf-8', errors='strict')`.
fn decode(args: &[i64]) -> i64 {
    codec_call(args, "decode")
}

/// Shared body of [`encode`] and [`decode`]: resolve the codec, then invoke
/// its `encode`/`decode` entry point.
fn codec_call(args: &[i64], member: &str) -> i64 {
    let Some(&object) = args.first() else {
        return type_error(&format!("{member}() missing required argument 'obj'"));
    };
    let encoding = match args.get(1).copied() {
        None => value::str_alloc("utf-8".to_string()),
        Some(value) => {
            if value::as_str(value).is_none() {
                return type_error(&format!(
                    "{member}() argument 'encoding' must be str, not {}",
                    value::type_name(value)
                ));
            }
            value
        }
    };
    let errors = match args.get(2).copied() {
        None => value::str_alloc("strict".to_string()),
        Some(value) => {
            if value::as_str(value).is_none() {
                return type_error(&format!(
                    "{member}() argument 'errors' must be str, not {}",
                    value::type_name(value)
                ));
            }
            value
        }
    };
    let info = lookup(&[encoding]);
    if exc::pending() {
        return NONE;
    }
    let entry =
        if value::instance_class_value(info) == Some(crate::modules::codecs_codecinfo_class()) {
            crate::core::methods::getattr(info, member.as_ptr(), member.len())
        } else if let Some(items) = value::as_tuple(info) {
            if items.len() != 4 {
                return type_error("codec registry entry is not a 4-tuple");
            }
            if member == "encode" {
                items[0]
            } else {
                items[1]
            }
        } else {
            return type_error("codec registry entry is not a 4-tuple");
        };
    if exc::pending() {
        return NONE;
    }
    let roots = heap::root_values(&[info, object, errors, entry]);
    // A built-in `_codecs` entry is invoked through this module directly. The
    // central `_codecs` specs cap the positional count below the trailing
    // `final` flag the stateless decoder protocol defines (`ascii_decode` and
    // `latin_1_decode` stop at two arguments, `utf_8_encode` at one), so the
    // module dispatcher would reject the trailing flag. Decoding treats its
    // whole input as final — an incomplete trailing sequence is a
    // `UnicodeDecodeError`, not silently unconsumed — and encoding takes
    // `(object, errors)` like CPython's `_codecs.encode`.
    let native_entry = value::as_function(entry).and_then(|function| match &function.body {
        value::FunctionBody::Native { module, name } if *module == "_codecs" => Some(*name),
        _ => None,
    });
    let result = if let Some(function) = native_entry {
        let call_args = [object, errors, value::bool_value(true)];
        let count = if member == "decode" { 3 } else { 2 };
        call(function, &call_args[..count])
    } else {
        let call_args = [object, errors];
        core_builtins::call(entry, call_args.len(), call_args.as_ptr())
    };
    let output = if exc::pending() {
        NONE
    } else if let Some(items) = value::as_tuple(result) {
        items.first().copied().unwrap_or(NONE)
    } else {
        result
    };
    heap::release_roots(roots);
    output
}

/// Encodes `text` with `codec` under `mode`.
fn encode_text(text: &str, codec: Codec, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    encode_codes(codec, text, mode)
}

/// Decodes `data` with `codec`, applying `mode` to bytes the codec rejects.
fn decode_bytes(data: &[u8], codec: Codec, mode: &ErrorMode) -> Result<String, i64> {
    decode_codes(codec, data, mode, true).map(|(codes, _)| value::str_from_codes(codes))
}

// Filesystem names (`os.fsdecode`/`os.fsencode`)

/// Decodes a filesystem name the way POSIX `os.fsdecode` does.
///
/// The filesystem encoding is UTF-8 under `surrogateescape`, which accepts
/// every byte sequence: an undecodable byte `b` becomes code point `0xDC00+b`.
pub(crate) fn fsdecode(data: &[u8]) -> String {
    match decode_codes(Codec::Utf8, data, &ErrorMode::SurrogateEscape, true) {
        Ok((codes, _)) => value::str_from_codes(codes),
        // `surrogateescape` rejects nothing, so this arm cannot be reached;
        // keep the helper infallible for its `os` callers.
        Err(_) => String::new(),
    }
}

/// Encodes a filesystem name the way POSIX `os.fsencode` does.
///
/// The filesystem encoding is UTF-8 under `surrogateescape`, so code point
/// `0xDC00+b` is written back as the byte `b`.
pub(crate) fn fsencode(text: &str) -> Vec<u8> {
    match encode_codes(Codec::Utf8, text, &ErrorMode::SurrogateEscape) {
        Ok(bytes) => bytes,
        // `surrogateescape` rejects nothing; keep the helper infallible.
        Err(_) => text.as_bytes().to_vec(),
    }
}

/// `ascii_encode(text, errors='strict')` -> `(bytes, length)`.
fn ascii_encode(args: &[i64]) -> i64 {
    single_byte_encode(args, Codec::Ascii, "ascii_encode")
}

/// `latin_1_encode(text, errors='strict')` -> `(bytes, length)`.
fn latin_1_encode(args: &[i64]) -> i64 {
    single_byte_encode(args, Codec::Latin1, "latin_1_encode")
}

fn single_byte_encode(args: &[i64], codec: Codec, function: &str) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return type_error(&format!(
            "{function}() argument 1 must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let bytes = match encode_single_byte(text, codec, &mode) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let length = ops::int_result(text.chars().count() as i64);
    let encoded = value::bytes_new(bytes);
    value::tuple_new(vec![encoded, length])
}

/// `ascii_decode(data, errors='strict')` -> `(text, consumed)`.
fn ascii_decode(args: &[i64]) -> i64 {
    single_byte_decode(args, Codec::Ascii, "ascii_decode")
}

/// `latin_1_decode(data, errors='strict')` -> `(text, consumed)`.
fn latin_1_decode(args: &[i64]) -> i64 {
    single_byte_decode(args, Codec::Latin1, "latin_1_decode")
}

fn single_byte_decode(args: &[i64], codec: Codec, function: &str) -> i64 {
    let data = match bytes_argument(function, args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let text = match decode_bytes(data, codec, &mode) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let consumed = ops::int_result(data.len() as i64);
    let text = value::str_alloc(text);
    value::tuple_new(vec![text, consumed])
}

fn utf_8_encode(args: &[i64]) -> i64 {
    encode_entry(args, Codec::Utf8, "utf_8_encode")
}

fn utf_8_decode(args: &[i64]) -> i64 {
    decode_entry(args, Codec::Utf8, "utf_8_decode")
}

/// Shared stateless encoder entry: `(text, errors='strict')` ->
/// `(bytes, length)`.
fn encode_entry(args: &[i64], codec: Codec, function: &str) -> i64 {
    let Some(text) = value::as_str(args[0]) else {
        return type_error(&format!(
            "{function}() argument 1 must be str, not {}",
            value::type_name(args[0])
        ));
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let bytes = match encode_codes(codec, text, &mode) {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let length = ops::int_result(value::str_codes(text).count() as i64);
    value::tuple_new(vec![value::bytes_new(bytes), length])
}

/// Shared stateless decoder entry: `(data, errors='strict'[, final])` ->
/// `(text, consumed)`.
fn decode_entry(args: &[i64], codec: Codec, function: &str) -> i64 {
    let data = match bytes_argument(function, args[0]) {
        Ok(data) => data,
        Err(error) => return error,
    };
    let errors = match optional_errors_argument(args, 1, function) {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let mode = match error_mode(errors) {
        Ok(mode) => mode,
        Err(error) => return error,
    };
    let final_ = args.get(2).copied().map(value::truthy).unwrap_or(false);
    match decode_codes(codec, data, &mode, final_) {
        Ok((codes, consumed)) => {
            let text = value::str_from_codes(codes);
            value::tuple_new(vec![
                value::str_alloc(text),
                ops::int_result(consumed as i64),
            ])
        }
        Err(error) => error,
    }
}

/// Encodes `text` as UTF-8, including the lone surrogates a runtime string
/// may hold.
fn encode_utf8(text: &str, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    let codes: Vec<u32> = value::str_codes(text).collect();
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < codes.len() {
        let code = codes[index];
        if !value::is_surrogate_code(code) {
            let character = char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buffer = [0u8; 4];
            out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 1;
        match mode {
            ErrorMode::SurrogatePass => {
                let (first, second, third) = surrogate_utf8_bytes(code);
                out.extend_from_slice(&[first, second, third]);
                index += 1;
                continue;
            }
            ErrorMode::SurrogateEscape => {
                if (0xDC80..=0xDCFF).contains(&code) {
                    out.push((code - 0xDC00) as u8);
                    index += 1;
                    continue;
                }
                return Err(unicode_encode_error(
                    Codec::Utf8,
                    text,
                    start,
                    end,
                    "surrogates not allowed",
                ));
            }
            // Consecutive unencodable code points share one report.
            ErrorMode::Strict | ErrorMode::Ignore | ErrorMode::Replace => {
                while end < codes.len() && value::is_surrogate_code(codes[end]) {
                    end += 1;
                }
            }
            _ => {}
        }
        match mode {
            ErrorMode::Strict => {
                return Err(unicode_encode_error(
                    Codec::Utf8,
                    text,
                    start,
                    end,
                    "surrogates not allowed",
                ));
            }
            ErrorMode::Ignore => {}
            ErrorMode::Replace => out.resize(out.len() + (end - start), b'?'),
            ErrorMode::BackslashReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(escaped_code(code).as_bytes());
                }
            }
            ErrorMode::NameReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(escaped_code(code).as_bytes());
                }
            }
            ErrorMode::XmlCharrefReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(format!("&#{code};").as_bytes());
                }
            }
            ErrorMode::Custom(handler) => {
                let exception =
                    encode_exception_value(Codec::Utf8, text, start, end, "surrogates not allowed");
                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                if exc::pending() {
                    return Err(result);
                }
                let (replacement, position) = encode_handler_result(result)?;
                match replacement {
                    EncodeReplacement::Bytes(bytes) => out.extend_from_slice(bytes),
                    EncodeReplacement::Text(text) => {
                        for code in value::str_codes(text) {
                            match char::from_u32(code) {
                                Some(character) => {
                                    let mut buffer = [0u8; 4];
                                    out.extend_from_slice(
                                        character.encode_utf8(&mut buffer).as_bytes(),
                                    );
                                }
                                None => {
                                    return Err(unicode_encode_error(
                                        Codec::Utf8,
                                        text,
                                        start,
                                        end,
                                        "surrogates not allowed",
                                    ))
                                }
                            }
                        }
                    }
                }
                if position <= start as i64 || position > codes.len() as i64 {
                    return Err(exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("error handler returned invalid position {position}"),
                    ));
                }
                index = position as usize;
                continue;
            }
            ErrorMode::SurrogatePass | ErrorMode::SurrogateEscape => unreachable!(),
        }
        index = end;
    }
    Ok(out)
}

/// The generalized UTF-8 encoding of a lone surrogate, as CPython's
/// `surrogatepass` handler produces it.
fn surrogate_utf8_bytes(code: u32) -> (u8, u8, u8) {
    (
        0xE0 | ((code >> 12) as u8),
        0x80 | (((code >> 6) & 0x3F) as u8),
        0x80 | ((code & 0x3F) as u8),
    )
}

/// Decodes `data` as UTF-8 into Python code points.
///
/// Returns `(codes, consumed_bytes)`. With `final == false` an incomplete
/// trailing sequence is left unconsumed; `surrogatepass` accepts the
/// generalized surrogate forms instead of rejecting them.
fn decode_utf8(data: &[u8], mode: &ErrorMode, final_: bool) -> Result<(Vec<u32>, usize), i64> {
    let mut out = Vec::with_capacity(data.len());
    let mut offset = 0usize;
    while offset < data.len() {
        let first = data[offset];
        if first < 0x80 {
            out.push(first as u32);
            offset += 1;
            continue;
        }
        let (width, minimum) = match first {
            0xC2..=0xDF => (2usize, 0x80u32),
            0xE0..=0xEF => (3, 0x800),
            0xF0..=0xF4 => (4, 0x10000),
            _ => (0, 0),
        };
        if width == 0 {
            offset = decode_error(
                Codec::Utf8,
                data,
                offset,
                offset + 1,
                decode_reason(first),
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        if offset + width > data.len() {
            if !final_ {
                return Ok((out, offset));
            }
            offset = decode_error(
                Codec::Utf8,
                data,
                offset,
                data.len(),
                "unexpected end of data",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        let mut code = (first as u32) & (0x7F >> width);
        let mut valid = true;
        for step in 1..width {
            let continuation = data[offset + step];
            if continuation & 0xC0 != 0x80 {
                valid = false;
                break;
            }
            code = (code << 6) | (continuation as u32 & 0x3F);
        }
        if !valid || code < minimum || code > 0x10_FFFF {
            offset = decode_error(
                Codec::Utf8,
                data,
                offset,
                offset + width,
                decode_reason(first),
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        if value::is_surrogate_code(code) && !matches!(mode, ErrorMode::SurrogatePass) {
            offset = decode_error(
                Codec::Utf8,
                data,
                offset,
                offset + width,
                "invalid continuation byte",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        out.push(code);
        offset += width;
    }
    Ok((out, offset))
}

/// Applies `mode` to one rejected input span and returns the resume offset.
///
/// `final_` only matters for the incomplete-sequence case some codecs report
/// as `unexpected end of data`.
#[allow(clippy::too_many_arguments)]
fn decode_error(
    codec: Codec,
    data: &[u8],
    start: usize,
    end: usize,
    reason: &str,
    mode: &ErrorMode,
    final_: bool,
    out: &mut Vec<u32>,
) -> Result<usize, i64> {
    let _ = final_;
    let width = end.saturating_sub(start).max(1);
    match mode {
        ErrorMode::Strict | ErrorMode::SurrogatePass => {
            Err(unicode_decode_error(codec, data, start, end, reason))
        }
        ErrorMode::Ignore => Ok(end.max(start + 1)),
        ErrorMode::Replace => {
            out.push(0xFFFD);
            Ok(end.max(start + 1))
        }
        ErrorMode::SurrogateEscape => {
            for byte in &data[start..end.max(start + 1)] {
                out.push(0xDC00 + u32::from(*byte));
            }
            Ok(end.max(start + 1))
        }
        ErrorMode::BackslashReplace => {
            for byte in &data[start..end.max(start + 1)] {
                push_escaped_byte(out, *byte);
            }
            Ok(end.max(start + 1))
        }
        ErrorMode::NameReplace | ErrorMode::XmlCharrefReplace => Err(type_error(
            "don't know how to handle UnicodeDecodeError in error callback",
        )),
        ErrorMode::Custom(handler) => {
            let exception = decode_exception_value(codec, data, start, end, reason);
            let result = core_builtins::call(*handler, 1, [exception].as_ptr());
            if exc::pending() {
                return Err(result);
            }
            let (replacement, position) = error_handler_result(result)?;
            if position <= start as i64 || position > data.len() as i64 {
                return Err(exc::raise_with_message(
                    VALUE_ERROR,
                    &format!("error handler returned invalid position {position}"),
                ));
            }
            out.extend(value::str_codes(replacement));
            let _ = width;
            Ok(position as usize)
        }
    }
}

/// Appends the `\xNN` escape of one byte to a code-point buffer.
fn push_escaped_byte(out: &mut Vec<u32>, byte: u8) {
    out.push(u32::from(b'\\'));
    out.push(u32::from(b'x'));
    for nibble in [byte >> 4, byte & 0x0F] {
        out.push(u32::from(hex_digit(nibble)));
    }
}

/// Lowercase hexadecimal digit for `value` (`0..=15`).
fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'a' + (value - 10),
    }
}

/// Strict/ignore/replace/surrogateescape/custom-handler ASCII decoding.
///
/// CPython reports every rejected byte on its own, so the error span is
/// always one byte wide. Latin-1 never reaches this path.
fn decode_ascii(data: &[u8], mode: &ErrorMode, final_: bool) -> Result<Vec<u32>, i64> {
    const REASON: &str = "ordinal not in range(128)";
    let mut out = Vec::with_capacity(data.len());
    let mut offset = 0usize;
    while offset < data.len() {
        let byte = data[offset];
        if byte < 0x80 {
            out.push(u32::from(byte));
            offset += 1;
            continue;
        }
        offset = decode_error(
            Codec::Ascii,
            data,
            offset,
            offset + 1,
            REASON,
            mode,
            final_,
            &mut out,
        )?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cross-codec engines
// ---------------------------------------------------------------------------

/// The byte a `surrogateescape` code point stands for.
///
/// PEP 383 maps undecodable byte `b` to code point `0xDC00 + b`; the byte
/// range that can be escaped this way is `0x80..=0xFF`.
fn surrogate_escape_byte(code: u32) -> Option<u8> {
    (0xDC80..=0xDCFF)
        .contains(&code)
        .then(|| (code - 0xDC00) as u8)
}

/// Whether this host stores UTF-16/UTF-32 code units big-endian first.
const NATIVE_BIG_ENDIAN: bool = cfg!(target_endian = "big");

/// Encodes `text` with `codec` under `mode`.
fn encode_codes(codec: Codec, text: &str, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    match codec {
        Codec::Utf8 => encode_utf8(text, mode),
        Codec::Utf8Sig => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(&encode_utf8(text, mode)?);
            Ok(out)
        }
        Codec::Ascii | Codec::Latin1 | Codec::Charmap => encode_single_byte(text, codec, mode),
        Codec::Utf16 => encode_utf16(codec, text, NATIVE_BIG_ENDIAN, true, mode),
        Codec::Utf16Le => encode_utf16(codec, text, false, false, mode),
        Codec::Utf16Be => encode_utf16(codec, text, true, false, mode),
        Codec::Utf32 => Ok(encode_utf32(text, NATIVE_BIG_ENDIAN, true)),
        Codec::Utf32Le => Ok(encode_utf32(text, false, false)),
        Codec::Utf32Be => Ok(encode_utf32(text, true, false)),
        Codec::Utf7 => iconv_encode("UTF-7", codec, text, mode),
        Codec::UnicodeEscape => Ok(encode_unicode_escape(text, false)),
        Codec::RawUnicodeEscape => Ok(encode_unicode_escape(text, true)),
        Codec::Rot13 => Ok(rot13_bytes(text)),
        Codec::Undefined => Err(exc::raise_with_message(UNICODE_ERROR, "undefined encoding")),
        Codec::Charset { charset, .. } => iconv_encode(charset, codec, text, mode),
        Codec::Multibyte { prog } => multibyte_encode(prog, text, mode),
    }
}

/// Decodes `data` with `codec`, applying `mode` to the spans it rejects.
///
/// With `final_ == false` an incomplete trailing sequence is left unconsumed,
/// as the incremental codec contract requires.
fn decode_codes(
    codec: Codec,
    data: &[u8],
    mode: &ErrorMode,
    final_: bool,
) -> Result<(Vec<u32>, usize), i64> {
    match codec {
        Codec::Utf8 => decode_utf8(data, mode, final_),
        Codec::Utf8Sig => decode_utf8_sig(data, mode, final_),
        Codec::Ascii => decode_ascii(data, mode, final_).map(|codes| (codes, data.len())),
        // Latin-1 (and the mapping-less `charmap`) is a total byte-to-code
        // point mapping, so no error handler can be consulted.
        Codec::Latin1 | Codec::Charmap => Ok((
            data.iter().map(|byte| u32::from(*byte)).collect(),
            data.len(),
        )),
        Codec::Utf16 => decode_utf16(data, codec, mode, final_, NATIVE_BIG_ENDIAN, true),
        Codec::Utf16Le => decode_utf16(data, codec, mode, final_, false, false),
        Codec::Utf16Be => decode_utf16(data, codec, mode, final_, true, false),
        Codec::Utf32 => decode_utf32(data, codec, mode, final_, NATIVE_BIG_ENDIAN, true),
        Codec::Utf32Le => decode_utf32(data, codec, mode, final_, false, false),
        Codec::Utf32Be => decode_utf32(data, codec, mode, final_, true, false),
        Codec::Utf7 => iconv_decode("UTF-7", codec, data, mode, final_),
        Codec::UnicodeEscape => decode_unicode_escape(data, codec, false, mode, final_),
        Codec::RawUnicodeEscape => decode_unicode_escape(data, codec, true, mode, final_),
        Codec::Rot13 => Ok((rot13_codes(data), data.len())),
        Codec::Undefined => Err(exc::raise_with_message(UNICODE_ERROR, "undefined encoding")),
        Codec::Charset { charset, .. } => iconv_decode(charset, codec, data, mode, final_),
        Codec::Multibyte { prog } => multibyte_decode(prog, data, mode, final_),
    }
}

/// Encodes `text` as UTF-16, optionally preceded by a byte-order mark.
///
/// UTF-16 strict encoding rejects lone surrogates, matching CPython's
/// `UnicodeEncodeError`; `surrogatepass` still writes them as code units.
fn encode_utf16(
    codec: Codec,
    text: &str,
    big_endian: bool,
    bom: bool,
    mode: &ErrorMode,
) -> Result<Vec<u8>, i64> {
    let mut out = Vec::with_capacity(text.len() * 2 + 2);
    if bom {
        out.extend_from_slice(if big_endian {
            &[0xFE, 0xFF]
        } else {
            &[0xFF, 0xFE]
        });
    }
    let codes: Vec<u32> = value::str_codes(text).collect();
    let mut index = 0usize;
    while index < codes.len() {
        let code = codes[index];
        if value::is_surrogate_code(code) && matches!(mode, ErrorMode::Strict) {
            return Err(unicode_encode_error(
                codec,
                text,
                index,
                index + 1,
                "surrogates not allowed",
            ));
        }
        let mut units = [0u16; 2];
        let count = if code <= 0xFFFF {
            units[0] = code as u16;
            1
        } else {
            let offset = code - 0x1_0000;
            units[0] = 0xD800 + ((offset >> 10) as u16 & 0x3FF);
            units[1] = 0xDC00 + (offset as u16 & 0x3FF);
            2
        };
        for unit in &units[..count] {
            if big_endian {
                out.extend_from_slice(&unit.to_be_bytes());
            } else {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        index += 1;
    }
    Ok(out)
}

/// Encodes `text` as UTF-32, optionally preceded by a byte-order mark.
fn encode_utf32(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 4 + 4);
    if bom {
        out.extend_from_slice(if big_endian {
            &[0x00, 0x00, 0xFE, 0xFF]
        } else {
            &[0xFF, 0xFE, 0x00, 0x00]
        });
    }
    for code in value::str_codes(text) {
        if big_endian {
            out.extend_from_slice(&code.to_be_bytes());
        } else {
            out.extend_from_slice(&code.to_le_bytes());
        }
    }
    out
}

/// Decodes UTF-16, honouring and stripping a byte-order mark when `bom` is
/// set (`utf-16`); the explicit endian variants pass their own order.
fn decode_utf16(
    data: &[u8],
    codec: Codec,
    mode: &ErrorMode,
    final_: bool,
    native_big_endian: bool,
    bom: bool,
) -> Result<(Vec<u32>, usize), i64> {
    let mut offset = 0usize;
    let mut big_endian = native_big_endian;
    if bom {
        if data.len() >= 2 {
            match (data[0], data[1]) {
                (0xFF, 0xFE) => {
                    big_endian = false;
                    offset = 2;
                }
                (0xFE, 0xFF) => {
                    big_endian = true;
                    offset = 2;
                }
                _ => {}
            }
        } else if !final_ && data.len() == 1 && (data[0] == 0xFF || data[0] == 0xFE) {
            // A split byte-order mark stays unconsumed for the next call.
            return Ok((Vec::new(), 0));
        }
    }
    let unit = |position: usize| -> u16 {
        let bytes = [data[position], data[position + 1]];
        if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    };
    let mut out = Vec::with_capacity(data.len() / 2);
    while offset < data.len() {
        if data.len() - offset < 2 {
            if !final_ {
                return Ok((out, offset));
            }
            offset = decode_error(
                codec,
                data,
                offset,
                data.len(),
                "unexpected end of data",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        let first = unit(offset);
        if (0xD800..=0xDBFF).contains(&first) {
            if data.len() - offset < 4 {
                if !final_ {
                    return Ok((out, offset));
                }
                offset = decode_error(
                    codec,
                    data,
                    offset,
                    data.len(),
                    "unexpected end of data",
                    mode,
                    final_,
                    &mut out,
                )?;
                continue;
            }
            let second = unit(offset + 2);
            if (0xDC00..=0xDFFF).contains(&second) {
                let code =
                    0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00);
                out.push(code);
                offset += 4;
                continue;
            }
            offset = decode_error(
                codec,
                data,
                offset,
                offset + 2,
                "illegal UTF-16 surrogate",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        if (0xDC00..=0xDFFF).contains(&first) {
            offset = decode_error(
                codec,
                data,
                offset,
                offset + 2,
                "illegal UTF-16 surrogate",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        out.push(u32::from(first));
        offset += 2;
    }
    Ok((out, offset))
}

/// Decodes UTF-32, honouring and stripping a byte-order mark when `bom` is
/// set (`utf-32`).
fn decode_utf32(
    data: &[u8],
    codec: Codec,
    mode: &ErrorMode,
    final_: bool,
    native_big_endian: bool,
    bom: bool,
) -> Result<(Vec<u32>, usize), i64> {
    let mut offset = 0usize;
    let mut big_endian = native_big_endian;
    if bom {
        if data.len() >= 4 {
            match data[..4] {
                [0xFF, 0xFE, 0x00, 0x00] => {
                    big_endian = false;
                    offset = 4;
                }
                [0x00, 0x00, 0xFE, 0xFF] => {
                    big_endian = true;
                    offset = 4;
                }
                _ => {}
            }
        } else if !final_ && data.iter().all(|byte| matches!(byte, 0x00 | 0xFE | 0xFF)) {
            return Ok((Vec::new(), 0));
        }
    }
    let mut out = Vec::with_capacity(data.len() / 4);
    while offset < data.len() {
        if data.len() - offset < 4 {
            if !final_ {
                return Ok((out, offset));
            }
            offset = decode_error(
                codec,
                data,
                offset,
                data.len(),
                "unexpected end of data",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        let bytes = [
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ];
        let code = if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        };
        if code > 0x10_FFFF {
            offset = decode_error(
                codec,
                data,
                offset,
                offset + 4,
                "code point not in range(0x110000)",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        }
        out.push(code);
        offset += 4;
    }
    Ok((out, offset))
}

/// Decodes UTF-8-sig: an optional leading byte-order mark is stripped.
fn decode_utf8_sig(data: &[u8], mode: &ErrorMode, final_: bool) -> Result<(Vec<u32>, usize), i64> {
    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    if data.len() >= 3 {
        if data[..3] == BOM {
            let (codes, used) = decode_utf8(&data[3..], mode, final_)?;
            return Ok((codes, used + 3));
        }
    } else if !final_ && BOM.starts_with(data) {
        // A split byte-order mark stays unconsumed for the next call.
        return Ok((Vec::new(), 0));
    }
    decode_utf8(data, mode, final_)
}

/// Encodes `text` with the `unicode_escape` or `raw_unicode_escape` codec.
fn encode_unicode_escape(text: &str, raw: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for code in value::str_codes(text) {
        if raw {
            if code <= 0xFF {
                out.push(code as u8);
            } else if code <= 0xFFFF {
                out.extend_from_slice(format!("\\u{code:04x}").as_bytes());
            } else {
                out.extend_from_slice(format!("\\U{code:08x}").as_bytes());
            }
            continue;
        }
        match code {
            0x5C => out.extend_from_slice(b"\\\\"),
            0x0A => out.extend_from_slice(b"\\n"),
            0x0D => out.extend_from_slice(b"\\r"),
            0x09 => out.extend_from_slice(b"\\t"),
            0x20..=0x7E => out.push(code as u8),
            0x00..=0xFF => out.extend_from_slice(format!("\\x{code:02x}").as_bytes()),
            0x100..=0xFFFF => out.extend_from_slice(format!("\\u{code:04x}").as_bytes()),
            _ => out.extend_from_slice(format!("\\U{code:08x}").as_bytes()),
        }
    }
    out
}

/// Reads `count` hexadecimal digits at `start` into a code point.
fn read_hex_digits(data: &[u8], start: usize, count: usize) -> Option<u32> {
    let digits = data.get(start..start + count)?;
    let mut code = 0u32;
    for digit in digits {
        let value = (*digit as char).to_digit(16)?;
        code = code.checked_mul(16)? + value;
    }
    Some(code)
}

/// Recognizes a `\xNN`/`\uNNNN`/`\UNNNNNNNN` escape at `index`, returning the
/// code point and the escape's length. `raw_unicode_escape` has no `\xNN`.
fn hex_escape_at(data: &[u8], index: usize, raw: bool) -> Option<(u32, usize)> {
    match *data.get(index + 1)? {
        b'x' if !raw => read_hex_digits(data, index + 2, 2).map(|code| (code, 4)),
        b'u' => read_hex_digits(data, index + 2, 4).map(|code| (code, 6)),
        b'U' => read_hex_digits(data, index + 2, 8).map(|code| (code, 10)),
        _ => None,
    }
}

/// Decodes `unicode_escape` / `raw_unicode_escape` input into code points.
fn decode_unicode_escape(
    data: &[u8],
    codec: Codec,
    raw: bool,
    mode: &ErrorMode,
    final_: bool,
) -> Result<(Vec<u32>, usize), i64> {
    let mut out = Vec::with_capacity(data.len());
    let mut index = 0usize;
    while index < data.len() {
        let byte = data[index];
        if byte != b'\\' {
            out.push(u32::from(byte));
            index += 1;
            continue;
        }
        if let Some((code, length)) = hex_escape_at(data, index, raw) {
            out.push(code);
            index += length;
            continue;
        }
        let Some(&escape) = data.get(index + 1) else {
            index = decode_error(
                codec,
                data,
                index,
                data.len(),
                "\\ at end of string",
                mode,
                final_,
                &mut out,
            )?;
            continue;
        };
        if raw {
            // Only `\u`/`\U` escapes exist; anything else stays literal.
            out.push(u32::from(b'\\'));
            index += 1;
            continue;
        }
        match escape {
            b'\\' => {
                out.push(u32::from(b'\\'));
                index += 2;
            }
            b'\'' => {
                out.push(u32::from(b'\''));
                index += 2;
            }
            b'"' => {
                out.push(u32::from(b'"'));
                index += 2;
            }
            b'n' => {
                out.push(0x0A);
                index += 2;
            }
            b't' => {
                out.push(0x09);
                index += 2;
            }
            b'r' => {
                out.push(0x0D);
                index += 2;
            }
            b'a' => {
                out.push(0x07);
                index += 2;
            }
            b'b' => {
                out.push(0x08);
                index += 2;
            }
            b'f' => {
                out.push(0x0C);
                index += 2;
            }
            b'v' => {
                out.push(0x0B);
                index += 2;
            }
            b'\n' => index += 2,
            b'0'..=b'7' => {
                let mut code = 0u32;
                let mut position = index + 1;
                let mut digits = 0usize;
                while digits < 3 && position < data.len() && (b'0'..=b'7').contains(&data[position])
                {
                    code = code * 8 + u32::from(data[position] - b'0');
                    position += 1;
                    digits += 1;
                }
                out.push(code);
                index = position;
            }
            b'x' | b'u' | b'U' => {
                index = decode_error(
                    codec,
                    data,
                    index,
                    (index + 2).min(data.len()),
                    "truncated escape",
                    mode,
                    final_,
                    &mut out,
                )?;
            }
            _ => {
                // Unknown escapes keep the backslash; the character is
                // copied on the next iteration.
                out.push(u32::from(b'\\'));
                index += 1;
            }
        }
    }
    Ok((out, index))
}

/// The `rot-13` byte mapping (ASCII letters only).
fn rot13_byte(byte: u8) -> u8 {
    match byte {
        b'A'..=b'Z' => b'A' + (byte - b'A' + 13) % 26,
        b'a'..=b'z' => b'a' + (byte - b'a' + 13) % 26,
        other => other,
    }
}

/// Encodes text with `rot-13`, returning the rotated payload's bytes.
fn rot13_bytes(text: &str) -> Vec<u8> {
    text.as_bytes()
        .iter()
        .map(|byte| rot13_byte(*byte))
        .collect()
}

/// `rot-13` is its own inverse; the rotated payload keeps UTF-8 bytes.
fn rot13_codes(data: &[u8]) -> Vec<u32> {
    let rotated: Vec<u8> = data.iter().map(|byte| rot13_byte(*byte)).collect();
    match std::str::from_utf8(&rotated) {
        Ok(text) => value::str_codes(text).collect(),
        Err(_) => rotated.iter().map(|byte| u32::from(*byte)).collect(),
    }
}

/// The error-handler spelling `codecs_native`'s codec objects expect.
fn mode_name(mode: &ErrorMode) -> &'static str {
    match mode {
        ErrorMode::Strict => "strict",
        ErrorMode::Ignore => "ignore",
        ErrorMode::Replace => "replace",
        ErrorMode::SurrogateEscape => "surrogateescape",
        ErrorMode::SurrogatePass => "surrogatepass",
        ErrorMode::BackslashReplace => "backslashreplace",
        ErrorMode::NameReplace => "namereplace",
        ErrorMode::XmlCharrefReplace => "xmlcharrefreplace",
        // A registered handler has no name the accelerator modules know; the
        // built-in strict behavior is the honest fallback.
        ErrorMode::Custom(_) => "strict",
    }
}

/// The `_codecs_*` accelerator module that implements a multibyte codec.
fn multibyte_module(prog: &str) -> &'static str {
    match prog {
        "gb2312" | "gbk" | "gb18030" | "hz" => "_codecs_cn",
        "cp932" | "euc_jis_2004" | "euc_jisx0213" | "euc_jp" | "shift_jis" | "shift_jis_2004"
        | "shift_jisx0213" => "_codecs_jp",
        "cp949" | "euc_kr" | "johab" => "_codecs_kr",
        "big5" | "cp950" => "_codecs_tw",
        "big5hkscs" => "_codecs_hk",
        _ => "_codecs_iso2022",
    }
}

/// Resolves a multibyte codec to the codec object `codecs_native` builds.
fn multibyte_codec(prog: &str) -> Result<i64, i64> {
    let name = value::str_alloc(prog.to_string());
    let codec = crate::modules::codecs_native::call(multibyte_module(prog), "getcodec", &[name]);
    if exc::pending() {
        return Err(codec);
    }
    if codec == NONE {
        return Err(exc::raise_with_message(
            LOOKUP_ERROR,
            &format!("unknown encoding: {prog}"),
        ));
    }
    Ok(codec)
}

/// Encodes through a `codecs_native` codec object.
fn multibyte_encode(prog: &str, text: &str, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    let codec = multibyte_codec(prog)?;
    let roots = heap::root_values(&[codec]);
    let encoder = crate::core::methods::getattr(codec, b"encode".as_ptr(), 6);
    let text_value = value::str_alloc(text.to_string());
    let errors = value::str_alloc(mode_name(mode).to_string());
    let result = core_builtins::call(encoder, 2, [text_value, errors].as_ptr());
    let output = if exc::pending() {
        Err(result)
    } else {
        match value::as_tuple(result).and_then(|items| items.first().copied()) {
            Some(encoded) => match value::as_bytes(encoded) {
                Some(bytes) => Ok(bytes.to_vec()),
                None => Err(type_error("encoder did not return bytes")),
            },
            None => Err(type_error("encoder did not return a tuple")),
        }
    };
    heap::release_roots(roots);
    output
}

/// Decodes through a `codecs_native` codec object.
fn multibyte_decode(
    prog: &str,
    data: &[u8],
    mode: &ErrorMode,
    _final_: bool,
) -> Result<(Vec<u32>, usize), i64> {
    let codec = multibyte_codec(prog)?;
    let roots = heap::root_values(&[codec]);
    let decoder = crate::core::methods::getattr(codec, b"decode".as_ptr(), 6);
    let bytes = value::bytes_new(data.to_vec());
    let errors = value::str_alloc(mode_name(mode).to_string());
    let result = core_builtins::call(decoder, 2, [bytes, errors].as_ptr());
    let output = if exc::pending() {
        Err(result)
    } else {
        match value::as_tuple(result).and_then(|items| items.first().copied()) {
            Some(decoded) => match value::as_str(decoded) {
                Some(text) => Ok((value::str_codes(text).collect(), data.len())),
                None => Err(type_error("decoder did not return str")),
            },
            None => Err(type_error("decoder did not return a tuple")),
        }
    };
    heap::release_roots(roots);
    output
}

// ---------------------------------------------------------------------------
// iconv conversions (`utf-7` and the single-byte `Charset` codecs)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod iconv_ffi {
    use std::os::raw::{c_char, c_int, c_void};

    #[cfg_attr(target_vendor = "apple", link(name = "iconv"))]
    extern "C" {
        pub fn iconv_open(tocode: *const c_char, fromcode: *const c_char) -> *mut c_void;
        pub fn iconv(
            cd: *mut c_void,
            inbuf: *mut *mut c_char,
            inbytesleft: *mut usize,
            outbuf: *mut *mut c_char,
            outbytesleft: *mut usize,
        ) -> usize;
        pub fn iconv_close(cd: *mut c_void) -> c_int;
    }
}

/// Why a conversion stopped before consuming all input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvFault {
    /// The bytes at this input offset do not form a valid character.
    Illegal(usize),
    /// The trailing bytes are a prefix of a character; the offset is the
    /// consumed prefix.
    Incomplete(usize),
    /// iconv failed for a reason other than input validity.
    Internal,
}

/// An open iconv descriptor, closed on drop.
struct Converter {
    #[cfg(unix)]
    cd: *mut std::ffi::c_void,
    #[cfg(not(unix))]
    _private: (),
}

impl Converter {
    fn open(from: &str, to: &str) -> Option<Converter> {
        #[cfg(unix)]
        {
            let from = std::ffi::CString::new(from).ok()?;
            let to = std::ffi::CString::new(to).ok()?;
            let cd = unsafe { iconv_ffi::iconv_open(to.as_ptr(), from.as_ptr()) };
            if cd.is_null() || cd as usize == usize::MAX {
                return None;
            }
            Some(Converter { cd })
        }
        #[cfg(not(unix))]
        {
            let _ = (from, to);
            None
        }
    }

    /// Converts `input`, appending the produced bytes to `out`.
    fn convert_into(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), ConvFault> {
        #[cfg(unix)]
        {
            let mut pos = 0usize;
            let mut chunk = vec![0u8; input.len().saturating_mul(2).saturating_add(64).max(256)];
            loop {
                let mut in_ptr = input[pos..].as_ptr() as *mut std::os::raw::c_char;
                let mut in_left = input.len() - pos;
                let mut out_ptr = chunk.as_mut_ptr() as *mut std::os::raw::c_char;
                let mut out_left = chunk.len();
                let result = unsafe {
                    iconv_ffi::iconv(
                        self.cd,
                        &mut in_ptr,
                        &mut in_left,
                        &mut out_ptr,
                        &mut out_left,
                    )
                };
                // Read errno before any allocation below can overwrite it.
                let errno = if result == usize::MAX {
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EINVAL)
                } else {
                    0
                };
                let produced = chunk.len() - out_left;
                out.extend_from_slice(&chunk[..produced]);
                let consumed = (input.len() - pos) - in_left;
                pos += consumed;
                if result != usize::MAX {
                    if in_left == 0 {
                        return Ok(());
                    }
                    continue;
                }
                if errno == libc::E2BIG {
                    if chunk.len() >= (1 << 28) {
                        return Err(ConvFault::Internal);
                    }
                    chunk.resize(chunk.len().saturating_mul(2), 0);
                    continue;
                }
                if errno == libc::EILSEQ {
                    return Err(ConvFault::Illegal(pos));
                }
                if errno == libc::EINVAL {
                    return Err(ConvFault::Incomplete(pos));
                }
                return Err(ConvFault::Internal);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (input, out);
            Err(ConvFault::Internal)
        }
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            iconv_ffi::iconv_close(self.cd);
        }
    }
}

/// One-shot conversion, used per code point by the encoder.
fn convert_oneshot(from: &str, to: &str, input: &[u8]) -> Option<Vec<u8>> {
    let mut converter = Converter::open(from, to)?;
    let mut out = Vec::new();
    converter.convert_into(input, &mut out).ok()?;
    Some(out)
}

/// Appends the UTF-8 characters in `bytes` to a code-point buffer.
fn push_utf8_codes(out: &mut Vec<u32>, bytes: &[u8]) {
    for character in String::from_utf8_lossy(bytes).chars() {
        out.push(value::storage_char_to_code(character));
    }
}

/// Encodes one code point through the iconv charset, if it is representable.
fn encode_codepoint(charset: &str, code: u32) -> Option<Vec<u8>> {
    let character = char::from_u32(code)?;
    let mut buffer = [0u8; 4];
    convert_oneshot(
        "UTF-8",
        charset,
        character.encode_utf8(&mut buffer).as_bytes(),
    )
}

/// Encodes `text` through an iconv charset, one code point at a time.
///
/// `surrogateescape` maps an escaped code point back to its byte; every other
/// unrepresentable code point goes through the active error mode.
fn iconv_encode(charset: &str, codec: Codec, text: &str, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    const REASON: &str = "illegal multibyte sequence";
    let codes: Vec<u32> = value::str_codes(text).collect();
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < codes.len() {
        let code = codes[index];
        if matches!(mode, ErrorMode::SurrogateEscape) {
            if let Some(byte) = surrogate_escape_byte(code) {
                out.push(byte);
                index += 1;
                continue;
            }
        }
        if let Some(bytes) = encode_codepoint(charset, code) {
            out.extend_from_slice(&bytes);
            index += 1;
            continue;
        }
        let start = index;
        let end = index + 1;
        match mode {
            ErrorMode::Strict | ErrorMode::SurrogateEscape | ErrorMode::SurrogatePass => {
                return Err(unicode_encode_error(codec, text, start, end, REASON));
            }
            ErrorMode::Ignore => {}
            ErrorMode::Replace => out.push(b'?'),
            ErrorMode::BackslashReplace => out.extend_from_slice(escaped_code(code).as_bytes()),
            ErrorMode::NameReplace => out.extend_from_slice(namereplace_escape(code).as_bytes()),
            ErrorMode::XmlCharrefReplace => {
                out.extend_from_slice(format!("&#{code};").as_bytes());
            }
            ErrorMode::Custom(handler) => {
                let exception = encode_exception_value(codec, text, start, end, REASON);
                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                if exc::pending() {
                    return Err(result);
                }
                let (replacement, position) = encode_handler_result(result)?;
                if position <= start as i64 || position > codes.len() as i64 {
                    return Err(exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("error handler returned invalid position {position}"),
                    ));
                }
                match replacement {
                    EncodeReplacement::Bytes(bytes) => out.extend_from_slice(bytes),
                    EncodeReplacement::Text(replacement_text) => {
                        for replacement_code in value::str_codes(replacement_text) {
                            match encode_codepoint(charset, replacement_code) {
                                Some(bytes) => out.extend_from_slice(&bytes),
                                None => {
                                    return Err(unicode_encode_error(
                                        codec, text, start, end, REASON,
                                    ))
                                }
                            }
                        }
                    }
                }
                index = position as usize;
                continue;
            }
        }
        index = end;
    }
    Ok(out)
}

/// Decodes an iconv charset into code points, applying `mode` to rejected
/// spans.
fn iconv_decode(
    charset: &str,
    codec: Codec,
    data: &[u8],
    mode: &ErrorMode,
    final_: bool,
) -> Result<(Vec<u32>, usize), i64> {
    let mut out: Vec<u32> = Vec::with_capacity(data.len());
    let mut offset = 0usize;
    while offset < data.len() {
        let Some(mut converter) = Converter::open(charset, "UTF-8") else {
            return Err(exc::raise_with_message(
                LOOKUP_ERROR,
                &format!("unknown encoding: {}", codec.name()),
            ));
        };
        let mut produced = Vec::new();
        match converter.convert_into(&data[offset..], &mut produced) {
            Ok(()) => {
                push_utf8_codes(&mut out, &produced);
                return Ok((out, data.len()));
            }
            Err(ConvFault::Illegal(position)) => {
                push_utf8_codes(&mut out, &produced);
                let start = offset + position.min(data.len() - offset);
                if start >= data.len() {
                    return Ok((out, data.len()));
                }
                let next = decode_error(
                    codec,
                    data,
                    start,
                    start + 1,
                    "illegal multibyte sequence",
                    mode,
                    final_,
                    &mut out,
                )?;
                offset = if next > start { next } else { start + 1 };
            }
            Err(ConvFault::Incomplete(position)) => {
                push_utf8_codes(&mut out, &produced);
                let consumed = (offset + position).min(data.len());
                if !final_ {
                    return Ok((out, consumed));
                }
                let next = decode_error(
                    codec,
                    data,
                    consumed,
                    data.len(),
                    "unexpected end of data",
                    mode,
                    final_,
                    &mut out,
                )?;
                if next <= consumed || next >= data.len() {
                    return Ok((out, data.len()));
                }
                offset = next;
            }
            Err(ConvFault::Internal) => {
                return Err(exc::raise_with_message(
                    RUNTIME_ERROR,
                    "codec conversion failed",
                ));
            }
        }
    }
    Ok((out, offset))
}

/// Encodes `text` with a single-byte codec (`ascii`, `latin-1` or `charmap`
/// with no mapping).
///
/// Consecutive unrepresentable code points share one error span, matching
/// CPython's report. `surrogateescape` examines each code point on its own:
/// `0xDC80..=0xDCFF` becomes the byte it stands for and anything else
/// unrepresentable raises.
fn encode_single_byte(text: &str, codec: Codec, mode: &ErrorMode) -> Result<Vec<u8>, i64> {
    let limit = codec.limit();
    let reason = format!("ordinal not in range({})", limit + 1);
    let codes: Vec<u32> = value::str_codes(text).collect();
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < codes.len() {
        let code = codes[index];
        if code <= limit {
            out.push(code as u8);
            index += 1;
            continue;
        }
        if matches!(mode, ErrorMode::SurrogateEscape) && (0xDC80..=0xDCFF).contains(&code) {
            out.push((code - 0xDC00) as u8);
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index + 1;
        // `surrogateescape` treats every unrepresentable code point on its
        // own, so it must not lump the adjacent run into one span.
        if !matches!(mode, ErrorMode::SurrogateEscape | ErrorMode::Custom(_)) {
            while end < codes.len() && codes[end] > limit {
                end += 1;
            }
        }
        match mode {
            ErrorMode::Strict | ErrorMode::SurrogateEscape | ErrorMode::SurrogatePass => {
                return Err(unicode_encode_error(codec, text, start, end, &reason));
            }
            ErrorMode::Ignore => {}
            ErrorMode::Replace => {
                // One '?' per unrepresentable code point, like CPython.
                out.resize(out.len() + (end - start), b'?');
            }
            ErrorMode::BackslashReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(escaped_code(code).as_bytes());
                }
            }
            ErrorMode::NameReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(namereplace_escape(code).as_bytes());
                }
            }
            ErrorMode::XmlCharrefReplace => {
                for &code in &codes[start..end] {
                    out.extend_from_slice(format!("&#{code};").as_bytes());
                }
            }
            ErrorMode::Custom(handler) => {
                let exception = encode_exception_value(codec, text, start, end, &reason);
                let result = core_builtins::call(*handler, 1, [exception].as_ptr());
                if exc::pending() {
                    return Err(result);
                }
                let (replacement, position) = encode_handler_result(result)?;
                if position <= start as i64 || position > codes.len() as i64 {
                    return Err(exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("error handler returned invalid position {position}"),
                    ));
                }
                match replacement {
                    EncodeReplacement::Bytes(bytes) => out.extend_from_slice(bytes),
                    EncodeReplacement::Text(replacement_text) => {
                        // A text replacement is encoded with the same codec; a
                        // code point it cannot represent raises for the
                        // original span, like CPython.
                        for replacement_code in value::str_codes(replacement_text) {
                            if replacement_code > limit {
                                return Err(unicode_encode_error(codec, text, start, end, &reason));
                            }
                            out.push(replacement_code as u8);
                        }
                    }
                }
                index = position as usize;
                continue;
            }
        }
        index = end;
    }
    Ok(out)
}

/// The `\N{...}` escape for `code`, falling back to `\uNNNN`/`\UNNNNNNNN`.
///
/// The runtime has no Unicode name database, so the numeric fallback is the
/// final answer for unknown names, exactly as CPython treats an unknown name.
fn namereplace_escape(code: u32) -> String {
    escaped_code(code)
}

/// Classifies the first byte of an invalid sequence like CPython's UTF-8
/// decoder.
fn decode_reason(byte: u8) -> &'static str {
    if (0x80..=0xBF).contains(&byte) || !(0xC2..=0xF4).contains(&byte) {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    }
}

fn unicode_decode_error(codec: Codec, data: &[u8], start: usize, _end: usize, reason: &str) -> i64 {
    let byte = data.get(start).copied().unwrap_or(0);
    exc::raise_with_message(
        UNICODE_DECODE_ERROR,
        &format!(
            "'{}' codec can't decode byte 0x{byte:02x} in position {start}: {reason}",
            codec.name()
        ),
    )
}

/// Builds a non-pending `UnicodeDecodeError` value for a custom handler.
///
/// The args tuple mirrors CPython's shape as closely as this runtime's
/// string-only exception arguments allow: `(encoding, object_repr, start,
/// end, reason)`.
fn decode_exception_value(
    codec: Codec,
    data: &[u8],
    start: usize,
    end: usize,
    reason: &str,
) -> i64 {
    let message = format!(
        "'{}' codec can't decode byte 0x{:02x} in position {start}: {reason}",
        codec.name(),
        data.get(start).copied().unwrap_or(0)
    );
    let args = vec![
        codec.name().to_string(),
        // The undecodable object, one lone surrogate per byte, so a handler
        // can recover the rejected span exactly.
        value::str_from_codes(data.iter().map(|byte| 0xDC00 + u32::from(*byte))),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_DECODE_ERROR, &message, args)
}

/// `UnicodeEncodeError` for the code point run starting at `start`.
fn unicode_encode_error(codec: Codec, text: &str, start: usize, _end: usize, reason: &str) -> i64 {
    let code = value::str_codes(text).nth(start).unwrap_or(0);
    exc::raise_with_message(
        UNICODE_ENCODE_ERROR,
        &format!(
            "'{}' codec can't encode character '{}' in position {start}: {reason}",
            codec.name(),
            escaped_code(code)
        ),
    )
}

/// Builds a non-pending `UnicodeEncodeError` value for a custom handler, with
/// the same `(encoding, object, start, end, reason)` shape CPython uses.
fn encode_exception_value(codec: Codec, text: &str, start: usize, end: usize, reason: &str) -> i64 {
    let code = value::str_codes(text).nth(start).unwrap_or(0);
    let message = format!(
        "'{}' codec can't encode character '{}' in position {start}: {reason}",
        codec.name(),
        escaped_code(code)
    );
    let args = vec![
        codec.name().to_string(),
        text.to_string(),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_ENCODE_ERROR, &message, args)
}

/// Renders a code point the way CPython's `UnicodeEncodeError` message does:
/// `\xNN`, `\uNNNN` or `\UNNNNNNNN`.
fn escaped_code(code: u32) -> String {
    if code <= 0xff {
        format!("\\x{code:02x}")
    } else if code <= 0xffff {
        format!("\\u{code:04x}")
    } else {
        format!("\\U{code:08x}")
    }
}

/// Validates a handler result: a `(replacement: str, new_position: int)`
/// tuple.
fn error_handler_result(result: i64) -> Result<(&'static str, i64), i64> {
    let Some(items) = value::as_tuple(result) else {
        return Err(type_error(
            "error handler must return a tuple of (str, int)",
        ));
    };
    if items.len() != 2 {
        return Err(type_error(
            "error handler must return a tuple of (str, int)",
        ));
    }
    let Some(replacement) = value::as_str(items[0]) else {
        return Err(type_error("error handler replacement must be a str"));
    };
    let Some(position) = value::as_integer(items[1]) else {
        return Err(type_error("error handler position must be an int"));
    };
    Ok((replacement, position))
}

/// The replacement of an encode error handler.
enum EncodeReplacement {
    /// Text encoded with the target codec (the shape the runtime's own
    /// handlers return).
    Text(&'static str),
    /// Raw bytes appended verbatim, as CPython's encoder protocol specifies.
    Bytes(&'static [u8]),
}

/// Validates an encode handler result: `(replacement: str | bytes,
/// new_position: int)`.
///
/// CPython's encoder handlers return bytes; accepting a `str` as well keeps
/// the handlers written against this runtime working.
fn encode_handler_result(result: i64) -> Result<(EncodeReplacement, i64), i64> {
    let Some(items) = value::as_tuple(result) else {
        return Err(type_error(
            "error handler must return a tuple of (str or bytes, int)",
        ));
    };
    if items.len() != 2 {
        return Err(type_error(
            "error handler must return a tuple of (str or bytes, int)",
        ));
    }
    let replacement = if let Some(text) = value::as_str(items[0]) {
        EncodeReplacement::Text(text)
    } else if let Some(bytes) = value::as_bytes(items[0]) {
        EncodeReplacement::Bytes(bytes)
    } else {
        return Err(type_error(
            "error handler replacement must be a str or bytes",
        ));
    };
    let Some(position) = value::as_integer(items[1]) else {
        return Err(type_error("error handler position must be an int"));
    };
    Ok((replacement, position))
}

// escape codec

fn escape_encode(args: &[i64]) -> i64 {
    let Some(data) = value::as_bytes(args[0]) else {
        return type_error(&format!(
            "escape_encode() argument 1 must be bytes, not {}",
            value::type_name(args[0])
        ));
    };
    if let Some(errors) = args.get(1).copied() {
        if errors != NONE && value::as_str(errors).is_none() {
            return type_error(&format!(
                "escape_encode() argument 2 must be str or None, not {}",
                value::type_name(errors)
            ));
        }
    }
    let mut out = Vec::with_capacity(data.len() * 2);
    for &byte in data {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x20..=0x7e => out.push(byte),
            _ => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
        }
    }
    let escaped = value::bytes_new(out);
    let length = ops::int_result(data.len() as i64);
    value::tuple_new(vec![escaped, length])
}

fn escape_decode(args: &[i64]) -> i64 {
    // CPython's legacy `escape_decode` also accepts a `str`; interpret it as
    // its UTF-8 bytes.
    let owned;
    let data: &[u8] = if let Some(data) = value::as_bytes(args[0]) {
        data
    } else if let Some(text) = value::as_str(args[0]) {
        owned = text.as_bytes().to_vec();
        &owned
    } else {
        return type_error(&format!(
            "escape_decode() argument 1 must be bytes, not {}",
            value::type_name(args[0])
        ));
    };
    let errors = match args.get(1).copied() {
        None | Some(NONE) => "strict",
        Some(value) => match value::as_str(value) {
            Some(errors) => errors,
            None => {
                return type_error(&format!(
                    "escape_decode() argument 2 must be str or None, not {}",
                    value::type_name(value)
                ))
            }
        },
    };
    if !matches!(errors, "strict" | "ignore" | "replace") {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("decoding error; unknown error handling code: {errors}"),
        );
    }
    let strict = errors == "strict";
    let replace = errors == "replace";
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
        let byte = data[i];
        if byte != b'\\' {
            out.push(byte);
            i += 1;
            continue;
        }
        if i + 1 >= data.len() {
            return exc::raise_with_message(VALUE_ERROR, "Trailing \\ in string");
        }
        let escape = data[i + 1];
        match escape {
            b'\\' | b'\'' | b'"' => {
                out.push(escape);
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0c);
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'v' => {
                out.push(0x0b);
                i += 2;
            }
            b'0'..=b'7' => {
                let mut value: u32 = 0;
                let mut digits = 0usize;
                let mut j = i + 1;
                while digits < 3 && j < data.len() && (b'0'..=b'7').contains(&data[j]) {
                    value = value * 8 + u32::from(data[j] - b'0');
                    digits += 1;
                    j += 1;
                }
                out.push((value & 0xff) as u8);
                i = j;
            }
            b'x' => {
                let digits_start = i + 2;
                let mut j = digits_start;
                while j < data.len() && j < digits_start + 2 && data[j].is_ascii_hexdigit() {
                    j += 1;
                }
                let digits = j - digits_start;
                if digits == 2 {
                    let hex = std::str::from_utf8(&data[digits_start..j]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                    i = j;
                } else if strict {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        &format!("invalid \\x escape at position {i}"),
                    );
                } else {
                    if replace {
                        out.push(b'?');
                    }
                    i = j;
                }
            }
            _ => {
                out.push(b'\\');
                out.push(escape);
                i += 2;
            }
        }
    }
    let decoded = value::bytes_new(out);
    let length = ops::int_result(data.len() as i64);
    value::tuple_new(vec![decoded, length])
}

// registries

/// The marker stream/ incremental codec base classes. `codecs` exposes these
/// for `isinstance` checks; Hydro's streams are native objects, so an empty
/// cached class is the faithful identity.
pub(crate) fn stream_class(name: &str) -> i64 {
    let key = format!("codecs.class.{name}");
    if let Some(class) = crate::core::globals::registry_get(&key) {
        return class;
    }
    let class = value::class_new(name.to_string(), NONE, Vec::new());
    crate::core::globals::registry_set(&key, class);
    class
}

/// `getdecoder(encoding)` / `getencoder(encoding)`: the `decode`/`encode`
/// member of the `CodecInfo` returned by `lookup`.
fn codec_member(args: &[i64], member: &str) -> i64 {
    let info = lookup(&args[..1]);
    if exc::pending() {
        return info;
    }
    let roots = crate::heap::root_values(&[info]);
    let result = crate::core::methods::getattr(info, member.as_ptr(), member.len());
    crate::heap::release_roots(roots);
    result
}

/// `lookup(name)`: the built-in codecs (and their aliases) are answered
/// first; other names consult the registered search functions.
fn lookup(args: &[i64]) -> i64 {
    let name = match text_argument("lookup", "encoding", args[0]) {
        Ok(name) => name,
        Err(error) => return error,
    };
    if let Some(codec) = codec_for_name(name) {
        return codec_info(codec);
    }
    let search_functions: Vec<i64> = SEARCH_FUNCTIONS.with(|functions| functions.borrow().clone());
    let name_value = value::str_alloc(name.to_string());
    for function in search_functions {
        let result = core_builtins::call(function, 1, [name_value].as_ptr());
        if exc::pending() {
            return result;
        }
        if result == NONE {
            continue;
        }
        // A search function may return a plain 4-tuple or a
        // `codecs.CodecInfo` entry (what `encodings.search_function` builds).
        if value::instance_class_value(result) == Some(crate::modules::codecs_codecinfo_class()) {
            let encode = value::instance_attr(result, "encode").unwrap_or(NONE);
            let decode = value::instance_attr(result, "decode").unwrap_or(NONE);
            if !value::truthy(core_builtins::callable(encode))
                || !value::truthy(core_builtins::callable(decode))
            {
                return type_error("incompatible codecs in CodecInfo");
            }
            return result;
        }
        let Some(items) = value::as_tuple(result) else {
            return type_error("codec search functions must return 4-tuples");
        };
        if items.len() != 4 {
            return type_error("codec search functions must return 4-tuples");
        }
        return result;
    }
    lookup_error_message(&format!("unknown encoding: {name}"))
}

/// Cached `CodecInfo`-shaped tuple for a built-in codec:
/// `(encode, decode, None, None)`.
///
/// The encode/decode entries are the stateless per-codec functions, which
/// take `(input, errors='strict')`. The incremental codecs are absent
/// (`None`), which `CodecInfo` permits and which is honest: this runtime has
/// no incremental codec object.
fn codec_info(codec: Codec) -> i64 {
    let key = format!("_codecs.codec_info.{}", codec.name());
    if let Some(cached) = globals::registry_get(&key) {
        return cached;
    }
    let (encode_name, encode_arity, decode_name, decode_arity) = match codec {
        Codec::Utf8 => ("utf_8_encode", 1, "utf_8_decode", 3),
        Codec::Ascii => ("ascii_encode", 2, "ascii_decode", 2),
        Codec::Latin1 => ("latin_1_encode", 2, "latin_1_decode", 2),
        // Every other codec uses the generic `{prog}_encode`/`{prog}_decode`
        // entry points [`call`] dispatches back to this module's engines.
        other => {
            let encode: &'static str =
                Box::leak(format!("{}_encode", other.prog()).into_boxed_str());
            let decode: &'static str =
                Box::leak(format!("{}_decode", other.prog()).into_boxed_str());
            (encode, 2, decode, 3)
        }
    };
    let encode = value::native_function_new("_codecs", encode_name, encode_arity);
    let decode = value::native_function_new("_codecs", decode_name, decode_arity);
    let info = value::tuple_new(vec![encode, decode, NONE, NONE]);
    globals::registry_set(&key, info);
    info
}

fn register(args: &[i64]) -> i64 {
    let function = args[0];
    if !value::truthy(core_builtins::callable(function)) {
        return type_error("argument must be callable");
    }
    heap::add_thread_root(function);
    SEARCH_FUNCTIONS.with(|functions| functions.borrow_mut().push(function));
    NONE
}

fn unregister(args: &[i64]) -> i64 {
    // CPython's unregister never diagnoses its argument; it silently removes
    // matching callables.
    let function = args[0];
    SEARCH_FUNCTIONS.with(|functions| {
        functions
            .borrow_mut()
            .retain(|registered| *registered != function);
    });
    NONE
}

fn register_error(args: &[i64]) -> i64 {
    let name = match value::as_str(args[0]) {
        Some(name) => name.to_string(),
        None => {
            return type_error(&format!(
                "register_error() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    let handler = args[1];
    if !value::truthy(core_builtins::callable(handler)) {
        return type_error("handler must be callable");
    }
    heap::add_thread_root(handler);
    ERROR_HANDLERS.with(|handlers| {
        let mut handlers = handlers.borrow_mut();
        handlers.retain(|(registered, _)| registered.as_str() != name);
        handlers.push((name, handler));
    });
    NONE
}

/// `_codecs._unregister_error(errors)`: removes a custom error handler.
///
/// CPython 3.14 semantics: built-in handler names raise `ValueError` (they
/// cannot be unregistered), a custom name returns `True` when removed and
/// `False` when no handler was registered under it.
fn unregister_error(args: &[i64]) -> i64 {
    const BUILTIN: [&str; 8] = [
        "strict",
        "ignore",
        "replace",
        "xmlcharrefreplace",
        "backslashreplace",
        "namereplace",
        "surrogatepass",
        "surrogateescape",
    ];
    let name = match value::as_str(args[0]) {
        Some(name) => name.to_string(),
        None => {
            return type_error(&format!(
                "_unregister_error() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    if BUILTIN.contains(&name.as_str()) {
        return exc::raise_with_message(
            VALUE_ERROR,
            &format!("cannot un-register built-in error handler '{name}'"),
        );
    }
    let removed = ERROR_HANDLERS.with(|handlers| {
        let mut handlers = handlers.borrow_mut();
        match handlers
            .iter()
            .rposition(|(registered, _)| registered == &name)
        {
            Some(index) => {
                handlers.remove(index);
                true
            }
            None => false,
        }
    });
    value::bool_value(removed)
}

fn lookup_error(args: &[i64]) -> i64 {
    let name = match value::as_str(args[0]) {
        Some(name) => name,
        None => {
            return type_error(&format!(
                "lookup_error() argument 1 must be str, not {}",
                value::type_name(args[0])
            ))
        }
    };
    if let Some(handler) = registered_error(name) {
        return handler;
    }
    if let Some(handler) = builtin_error_handler(name) {
        return handler;
    }
    lookup_error_message(&format!("unknown error handler name '{name}'"))
}

/// Native function value for a built-in error handler, cached for identity.
fn builtin_error_handler(name: &str) -> Option<i64> {
    let cache_key = match name {
        "strict" => "_codecs.strict_errors",
        "ignore" => "_codecs.ignore_errors",
        "replace" => "_codecs.replace_errors",
        "surrogateescape" => "_codecs.surrogateescape_errors",
        _ => return None,
    };
    if let Some(cached) = globals::registry_get(cache_key) {
        return Some(cached);
    }
    let function = value::native_function_new("_codecs", leak_handler_name(name), 1);
    globals::registry_set(cache_key, function);
    Some(function)
}

/// `native_function_new` stores `&'static str` names. The four handler names
/// are fixed, so interning them once is enough.
fn leak_handler_name(name: &str) -> &'static str {
    match name {
        "strict" => "strict_errors",
        "ignore" => "ignore_errors",
        "surrogateescape" => "surrogateescape_errors",
        _ => "replace_errors",
    }
}

// Built-in error handlers

/// Extracts the `end` position from a `UnicodeDecodeError`/`UnicodeEncodeError`
/// created by this module (`args[3]`).
fn error_handler_end(exception: i64) -> Result<i64, i64> {
    let Some(data) = value::exception_data(exception) else {
        return Err(type_error(
            "error handler expects a UnicodeDecodeError or UnicodeEncodeError",
        ));
    };
    let Some(text) = data.args.get(3) else {
        return Err(type_error(
            "error handler expects a Unicode error with start/end args",
        ));
    };
    match text.parse::<i64>() {
        Ok(end) => Ok(end),
        Err(_) => Err(type_error(
            "error handler expects a Unicode error with start/end args",
        )),
    }
}

fn strict_errors(args: &[i64]) -> i64 {
    if value::exception_data(args[0]).is_none() {
        return type_error("strict_errors() argument must be an exception");
    }
    exc::raise_value(args[0])
}

fn ignore_errors(args: &[i64]) -> i64 {
    let end = match error_handler_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    let replacement = value::str_alloc(String::new());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}

fn replace_errors(args: &[i64]) -> i64 {
    let end = match error_handler_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    // CPython uses U+FFFD for undecodable bytes and '?' for characters the
    // target codec cannot encode.
    let replacement_text = match value::exception_data(args[0]).map(|data| data.tag) {
        Some(UNICODE_DECODE_ERROR) => "\u{FFFD}",
        _ => "?",
    };
    let replacement = value::str_alloc(replacement_text.to_string());
    let position = ops::int_result(end);
    value::tuple_new(vec![replacement, position])
}

/// Extracts the `start` position from a `UnicodeDecodeError`/
/// `UnicodeEncodeError` created by this module (`args[2]`).
fn error_handler_start(exception: i64) -> Result<i64, i64> {
    let Some(data) = value::exception_data(exception) else {
        return Err(type_error(
            "error handler expects a UnicodeDecodeError or UnicodeEncodeError",
        ));
    };
    let Some(text) = data.args.get(2) else {
        return Err(type_error(
            "error handler expects a Unicode error with start/end args",
        ));
    };
    text.parse::<i64>()
        .map_err(|_| type_error("error handler expects a Unicode error with start/end args"))
}

/// `surrogateescape_errors(exception)`: the PEP 383 handler returned by
/// `lookup_error('surrogateescape')`.
///
/// Decoding fails only for bytes that cannot be represented, so every byte
/// of the rejected span becomes the lone surrogate `0xDC00 + b` (the state
/// the decode exception carries as its object). Encoding maps
/// `0xDC80..=0xDCFF` back to its byte and re-raises for a code point outside
/// that range, mirroring CPython's PEP 383 handler.
fn surrogateescape_errors(args: &[i64]) -> i64 {
    let Some(data) = value::exception_data(args[0]) else {
        return type_error(
            "surrogateescape_errors() argument must be a UnicodeDecodeError \
             or UnicodeEncodeError",
        );
    };
    let tag = data.tag;
    let start = match error_handler_start(args[0]) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let end = match error_handler_end(args[0]) {
        Ok(end) => end,
        Err(error) => return error,
    };
    let count = (end - start).max(0) as usize;
    if tag == UNICODE_DECODE_ERROR {
        // The decode exception carries the undecodable object as its
        // surrogate-escaped text, so the rejected span is recoverable.
        let Some(object) = data.args.get(1) else {
            return type_error("error handler expects a Unicode error with start/end args");
        };
        let codes: Vec<u32> = value::str_codes(object)
            .skip(start.max(0) as usize)
            .take(count)
            .collect();
        let replacement = value::str_from_codes(codes);
        return value::tuple_new(vec![value::str_alloc(replacement), ops::int_result(end)]);
    }
    if tag == UNICODE_ENCODE_ERROR {
        // Encode errors carry the source text in `args[1]`, so the rejected
        // characters are recoverable. Collect the replacement bytes before
        // allocating anything (the exception payload borrow ends there).
        let Some(text) = data.args.get(1) else {
            return type_error("error handler expects a Unicode error with start/end args");
        };
        let mut bytes = Vec::with_capacity(count);
        for code in value::str_codes(text)
            .skip(start.max(0) as usize)
            .take(count)
        {
            match surrogate_escape_byte(code) {
                Some(byte) => bytes.push(byte),
                None => return exc::raise_value(args[0]),
            }
        }
        let encoded = value::bytes_new(bytes);
        return value::tuple_new(vec![encoded, ops::int_result(end)]);
    }
    type_error(
        "surrogateescape_errors() argument must be a UnicodeDecodeError \
         or UnicodeEncodeError",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resets the process-wide runtime state and returns the lock that
    /// serialises this module's tests (the global store is shared by all
    /// threads, so resets must not interleave).
    fn init() -> std::sync::MutexGuard<'static, ()> {
        let guard = crate::heap::heap_reset_test_guard();
        crate::core::globals::reset();
        exc::init();
        guard
    }

    fn text(s: &str) -> i64 {
        value::str_alloc(s.to_string())
    }

    fn bytes(data: &[u8]) -> i64 {
        value::bytes_new(data.to_vec())
    }

    fn str_of(v: i64) -> String {
        value::as_str(v).expect("str value").to_string()
    }

    fn bytes_of(v: i64) -> Vec<u8> {
        value::as_bytes(v).expect("bytes value").to_vec()
    }

    /// The Python code points of a string value.
    ///
    /// Lone surrogates are stored in the private-use mirror block, so a
    /// payload can never be compared, character by character, against a
    /// literal containing `\udcXX`; `str_codes` inverts the representation.
    fn codes_of(v: i64) -> Vec<u32> {
        let text = value::as_str(v).expect("str value");
        value::str_codes(text).collect()
    }

    /// A string value built from Python code points (surrogates included).
    fn text_codes(codes: &[u32]) -> i64 {
        value::str_alloc(value::str_from_codes(codes.iter().copied()))
    }

    fn info_entry(name: &str, index: usize) -> i64 {
        let info = call("lookup", &[text(name)]);
        assert!(!exc::pending(), "lookup({name}) must not fail");
        let items = value::as_tuple(info).expect("CodecInfo-shaped tuple");
        assert_eq!(items.len(), 4, "{name} entry");
        items[index]
    }

    #[test]
    fn ascii_round_trips_through_names_and_aliases() {
        let _guard = init();
        let encoded = call("encode", &[text("abc"), text("ascii")]);
        assert_eq!(bytes_of(encoded), b"abc");
        let decoded = call("decode", &[bytes(b"abc"), text("ascii")]);
        assert_eq!(str_of(decoded), "abc");
        for alias in ["ascii", "us-ascii", "US_ASCII", "646"] {
            let encoded = call("encode", &[text("hi"), text(alias)]);
            assert!(!exc::pending(), "{alias}");
            assert_eq!(bytes_of(encoded), b"hi");
            let decoded = call("decode", &[bytes(b"hi"), text(alias)]);
            assert_eq!(str_of(decoded), "hi", "{alias}");
        }
    }

    #[test]
    fn latin_1_decodes_every_byte_and_answers_aliases() {
        let _guard = init();
        for alias in [
            "latin-1",
            "latin1",
            "latin_1",
            "iso-8859-1",
            "ISO_8859_1",
            "l1",
        ] {
            let decoded = call("decode", &[bytes(&[0xff, 0x80, 0x00]), text(alias)]);
            assert!(!exc::pending(), "{alias}");
            assert_eq!(str_of(decoded), "\u{ff}\u{80}\u{0}", "{alias}");
        }
        let decoded = call("decode", &[bytes(&[0xe9]), text("latin-1")]);
        assert_eq!(str_of(decoded), "é");
        let encoded = call("encode", &[text("é"), text("latin-1")]);
        assert_eq!(bytes_of(encoded), vec![0xe9]);
        // The error-handler name is validated even though Latin-1 decoding
        // cannot fail.
        call("decode", &[bytes(&[0xff]), text("latin-1"), text("bogus")]);
        assert_eq!(exc::pending_tag(), i64::from(LOOKUP_ERROR));
    }

    #[test]
    fn ascii_decode_strict_ignore_and_replace() {
        let _guard = init();
        call("decode", &[bytes(b"a\xffb"), text("ascii")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_DECODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'ascii' codec can't decode byte 0xff in position 1: ordinal not in range(128)"
        );
        exc::clear();
        let decoded = call("decode", &[bytes(b"a\xffb"), text("ascii"), text("ignore")]);
        assert_eq!(str_of(decoded), "ab");
        let decoded = call(
            "decode",
            &[bytes(b"a\xffb"), text("ascii"), text("replace")],
        );
        assert_eq!(str_of(decoded), "a\u{fffd}b");
    }

    #[test]
    fn unrepresentable_characters_raise_and_honour_error_modes() {
        let _guard = init();
        call("encode", &[text("aé"), text("ascii")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'ascii' codec can't encode character '\\xe9' in position 1: \
             ordinal not in range(128)"
        );
        exc::clear();
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("aéé"), text("ascii"), text("ignore")]
            )),
            b"a"
        );
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("aéé"), text("ascii"), text("replace")]
            )),
            b"a??"
        );

        call("encode", &[text("€"), text("latin-1")]);
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "'latin-1' codec can't encode character '\\u20ac' in position 0: \
             ordinal not in range(256)"
        );
        exc::clear();
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("€"), text("latin-1"), text("replace")]
            )),
            b"?"
        );
        assert_eq!(
            bytes_of(call(
                "encode",
                &[text("€"), text("latin-1"), text("ignore")]
            )),
            b""
        );
    }

    #[test]
    fn stateless_entries_report_input_lengths() {
        let _guard = init();
        let encoded = call("ascii_encode", &[text("aé"), text("replace")]);
        let items = value::as_tuple(encoded).expect("tuple");
        assert_eq!(bytes_of(items[0]), b"a?");
        assert_eq!(value::as_integer(items[1]), Some(2));

        let decoded = call("latin_1_decode", &[bytes(&[0xff])]);
        let items = value::as_tuple(decoded).expect("tuple");
        assert_eq!(str_of(items[0]), "ÿ");
        assert_eq!(value::as_integer(items[1]), Some(1));

        // `None` means "strict" for the stateless decoders.
        let decoded = call("ascii_decode", &[bytes(b"ok"), NONE]);
        let items = value::as_tuple(decoded).expect("tuple");
        assert_eq!(str_of(items[0]), "ok");
        assert_eq!(value::as_integer(items[1]), Some(2));
    }

    #[test]
    fn lookup_names_built_in_codecs_and_rejects_unknown_names() {
        let _guard = init();
        for (alias, encode_name, decode_name) in [
            ("utf-8", "utf_8_encode", "utf_8_decode"),
            ("ascii", "ascii_encode", "ascii_decode"),
            ("latin_1", "latin_1_encode", "latin_1_decode"),
            ("iso-8859-1", "latin_1_encode", "latin_1_decode"),
            ("utf-16be", "utf_16_be_encode", "utf_16_be_decode"),
            ("utf-32le", "utf_32_le_encode", "utf_32_le_decode"),
        ] {
            let encode = info_entry(alias, 0);
            assert_eq!(
                value::as_function(encode).map(|function| function.name.as_str()),
                Some(encode_name),
                "{alias}"
            );
            let decode = info_entry(alias, 1);
            assert_eq!(
                value::as_function(decode).map(|function| function.name.as_str()),
                Some(decode_name),
                "{alias}"
            );
            assert_eq!(info_entry(alias, 2), NONE, "{alias} incremental encoder");
            assert_eq!(info_entry(alias, 3), NONE, "{alias} incremental decoder");
        }
        // Charset codecs are served through the system `iconv`, so `cp1252` is
        // a real codec now; only a name no table knows is a LookupError.
        let encode = info_entry("cp1252", 0);
        assert_eq!(
            value::as_function(encode).map(|function| function.name.as_str()),
            Some("cp1252_encode"),
            "cp1252 charset entry"
        );
        call("lookup", &[text("no-such-codec")]);
        assert_eq!(exc::pending_tag(), i64::from(LOOKUP_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "unknown encoding: no-such-codec"
        );
    }

    #[test]
    fn registered_handlers_apply_to_encode_and_decode() {
        let _guard = init();
        let handler = call("lookup_error", &[text("ignore")]);
        assert!(!exc::pending());
        assert_eq!(call("register_error", &[text("mine"), handler]), NONE);
        assert_eq!(
            str_of(call(
                "decode",
                &[bytes(b"a\xffb"), text("ascii"), text("mine")]
            )),
            "ab"
        );
        assert_eq!(
            bytes_of(call("encode", &[text("aé"), text("ascii"), text("mine")])),
            b"a"
        );
    }

    #[test]
    fn surrogateescape_decodes_undecodable_bytes_without_raising() {
        let _guard = init();
        let decoded = call(
            "decode",
            &[bytes(b"f\x80o\xff"), text("utf-8"), text("surrogateescape")],
        );
        assert!(!exc::pending());
        // PEP 383: undecodable byte `b` becomes code point `0xDC00 + b`.
        assert_eq!(codes_of(decoded), vec![0x66, 0xDC80, 0x6F, 0xDCFF]);
        assert_eq!(codes_of(decoded).last().copied(), Some(0xDCFF));

        // Every byte in 0x80..=0xff becomes one stand-in (one per byte).
        let invalid: Vec<u8> = (0x80..=0xff).collect();
        let decoded = call(
            "decode",
            &[bytes(&invalid), text("utf-8"), text("surrogateescape")],
        );
        let codes = codes_of(decoded);
        assert_eq!(codes.len(), invalid.len());
        assert_eq!(
            codes,
            invalid
                .iter()
                .map(|byte| 0xDC00 + u32::from(*byte))
                .collect::<Vec<_>>()
        );

        // A surrogate encoded as UTF-8 is escaped byte by byte.
        let decoded = call(
            "decode",
            &[
                bytes(b"\xed\xb0\x80"),
                text("utf-8"),
                text("surrogateescape"),
            ],
        );
        assert_eq!(codes_of(decoded), vec![0xDCED, 0xDCB0, 0xDC80]);

        // ASCII takes the same per-byte path.
        let decoded = call(
            "decode",
            &[bytes(b"a\xffb"), text("ascii"), text("surrogateescape")],
        );
        assert_eq!(codes_of(decoded), vec![0x61, 0xDCFF, 0x62]);
    }

    #[test]
    fn surrogateescape_encodes_the_stand_in_back_to_its_byte() {
        let _guard = init();
        let input: &[u8] = b"a\xffb";
        for codec in ["utf-8", "ascii"] {
            let decoded = call(
                "decode",
                &[bytes(input), text(codec), text("surrogateescape")],
            );
            assert!(!exc::pending(), "{codec}");
            assert_eq!(codes_of(decoded), vec![0x61, 0xDCFF, 0x62], "{codec}");
            let encoded = call("encode", &[decoded, text(codec), text("surrogateescape")]);
            assert!(!exc::pending(), "{codec}");
            assert_eq!(bytes_of(encoded), input, "{codec}");
        }

        // Latin-1 decodes every byte, so the handler only applies when
        // encoding: the stand-in still stands for 0xff.
        let decoded = call(
            "decode",
            &[bytes(input), text("latin-1"), text("surrogateescape")],
        );
        assert_eq!(str_of(decoded), "aÿb");
        let encoded = call(
            "encode",
            &[
                text_codes(&[0x61, 0xDCFF, 0x62]),
                text("latin-1"),
                text("surrogateescape"),
            ],
        );
        assert_eq!(bytes_of(encoded), input);

        // The exact acceptance case: one 0xff byte round-trips.
        let decoded = call(
            "decode",
            &[bytes(&[0xff]), text("utf-8"), text("surrogateescape")],
        );
        assert_eq!(codes_of(decoded), vec![0xDCFF]);
        let encoded = call("encode", &[decoded, text("utf-8"), text("surrogateescape")]);
        assert_eq!(bytes_of(encoded), vec![0xff]);

        // A truncated multi-byte sequence is escaped byte by byte too.
        let decoded = call(
            "decode",
            &[
                bytes(b"\xf0\x90\x80"),
                text("utf-8"),
                text("surrogateescape"),
            ],
        );
        assert_eq!(codes_of(decoded), vec![0xDCF0, 0xDC90, 0xDC80]);
    }

    #[test]
    fn surrogateescape_leaves_valid_multibyte_text_alone() {
        let _guard = init();
        let decoded = call(
            "decode",
            &[
                bytes("é€".as_bytes()),
                text("utf-8"),
                text("surrogateescape"),
            ],
        );
        assert_eq!(str_of(decoded), "é€");
        let encoded = call("encode", &[decoded, text("utf-8"), text("surrogateescape")]);
        assert_eq!(bytes_of(encoded), "é€".as_bytes());
    }

    #[test]
    fn surrogateescape_rejects_other_unrepresentable_characters() {
        let _guard = init();
        call(
            "encode",
            &[text("€"), text("ascii"), text("surrogateescape")],
        );
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        exc::clear();
        call(
            "encode",
            &[text("€"), text("latin-1"), text("surrogateescape")],
        );
        assert_eq!(exc::pending_tag(), i64::from(UNICODE_ENCODE_ERROR));
        exc::clear();
    }

    #[test]
    fn lookup_error_exposes_the_surrogateescape_handler() {
        let _guard = init();
        let handler = call("lookup_error", &[text("surrogateescape")]);
        assert!(!exc::pending());
        assert!(value::truthy(core_builtins::callable(handler)));

        let decoded = call(
            "decode",
            &[bytes(b"a\xffb"), text("utf-8"), text("surrogateescape")],
        );
        assert_eq!(codes_of(decoded), vec![0x61, 0xDCFF, 0x62]);
        let decoded = call(
            "decode",
            &[bytes(b"\xff"), text("ascii"), text("surrogateescape")],
        );
        assert_eq!(codes_of(decoded), vec![0xDCFF]);
        let encoded = call("encode", &[decoded, text("ascii"), text("surrogateescape")]);
        assert_eq!(bytes_of(encoded), b"\xff");
    }

    #[test]
    fn filesystem_helpers_use_utf8_with_surrogateescape() {
        let _guard = init();
        assert_eq!(fsdecode(b"plain"), "plain");
        assert_eq!(fsencode("plain"), b"plain");
        // The undecodable byte becomes its PEP 383 stand-in code point.
        let undecodable = fsdecode(b"name-\xff");
        assert_eq!(
            value::str_codes(&undecodable).collect::<Vec<_>>(),
            vec![0x6E, 0x61, 0x6D, 0x65, 0x2D, 0xDCFF]
        );
        assert_eq!(fsencode(&undecodable), b"name-\xff");
        assert_eq!(
            fsencode(&value::str_from_codes([
                0x6E, 0x61, 0x6D, 0x65, 0x2D, 0xDCFF
            ])),
            b"name-\xff"
        );
    }
}
