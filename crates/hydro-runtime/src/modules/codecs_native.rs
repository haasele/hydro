//! `_codecs_cn` / `_codecs_jp` / `_codecs_kr` / `_codecs_tw` /
//! `_codecs_hk` / `_codecs_iso2022` and `_multibytecodec`.
//!
//! The CPython `encodings` package imports the per-language accelerator for
//! every non-ASCII codec and asks it for a codec object with
//! `getcodec(name)`. That object exposes `encode`/`decode`, and the module
//! `_multibytecodec` supplies the incremental codec base classes the
//! `encodings/*.py` wrappers derive from. This module implements that
//! contract with real conversions:
//!
//! * stateless codecs (`gb2312`, `gbk`, `gb18030`, `big5`, `cp950`,
//!   `big5hkscs`, `shift_jis`, `cp932`, `euc_jp`, `euc_jisx0213`,
//!   `euc_jis_2004`, `shift_jisx0213`, `shift_jis_2004`, `euc_kr`, `cp949`,
//!   `johab`) convert through the C library `iconv` tables;
//! * `hz` and the ISO-2022 family are implemented here: their escape/shift
//!   state is not exposed by `iconv`, and the incremental objects must be
//!   able to resume in the middle of a designation;
//! * `MultibyteIncrementalEncoder`/`MultibyteIncrementalDecoder` keep their
//!   pending bytes and ISO-2022 shift state in hidden instance attributes, so
//!   `getstate`/`setstate` round-trip exactly like CPython's byte-packed
//!   state (including the layout `test_multibytecodec` asserts for
//!   `iso2022_jp`).
//!
//! Error handling follows CPython: invalid input reports `UnicodeDecodeError`
//! / `UnicodeEncodeError` with the codec's spelling and position, and the
//! `strict`/`ignore`/`replace`/custom handlers registered through
//! `codecs.register_error` are consulted through the `_codecs` registry.
//! ASCII bytes are never sent through `iconv`, so glibc's alternative ASCII
//! mappings (yen/overline in `SHIFT_JIS`) cannot leak into the result.
//!
//! The implementations are honest about their dependencies: `getcodec`
//! checks that the system iconv actually provides the required character
//! sets and raises `LookupError` otherwise. JIS X 0213 two-code-point pairs
//! are handled by iconv's `EUC-JISX0213`/`SHIFT_JISX0213` tables; the
//! JIS X 0213:2000 emulation CPython applies to `iso2022_jp_3` is not
//! reproduced (iconv maps those code points with the current JIS X 0213
//! tables). `euc_kr`'s KS X 1001 jamo make-up sequences are likewise left to
//! iconv.
//!
//! The module is wired by the central spec table: each module needs
//! `ModuleSpec` entries for `getcodec`/`__create_codec` and the class
//! dispatchers, plus `constant` values for the five `_multibytecodec`
//! classes. See the integration notes in the repository change description.

use std::ffi::c_void;
use std::os::raw::c_char;

use hydro_ir::MethodKind;

use crate::builtins as core_builtins;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, INDEX_ERROR, LOOKUP_ERROR, RUNTIME_ERROR, TYPE_ERROR,
    UNICODE_DECODE_ERROR, UNICODE_ENCODE_ERROR, UNICODE_ERROR,
};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

/// Module that owns the codec-object and incremental dispatchers.
const MBC: &str = "_multibytecodec";

/// Hidden attribute holding the codec name on a codec object and on an
/// incremental codec instance.
const NAME_ATTR: &str = "\u{0}mbc.name";
/// Hidden attribute holding the error-handler name of an incremental codec.
const ERRORS_ATTR: &str = "\u{0}mbc.errors";
/// Hidden attribute holding the packed ISO-2022 state of an incremental codec.
const STATE_ATTR: &str = "\u{0}mbc.state";
/// Hidden attribute holding the pending input of an incremental codec.
const PENDING_ATTR: &str = "\u{0}mbc.pending";
/// Hidden attribute holding the pending Unicode text of an incremental
/// encoder (JIS X 0213 pairs).
const PENDING_TEXT_ATTR: &str = "\u{0}mbc.pending_text";

// -------------------------------------------------------------------------
// Codec catalogue
// -------------------------------------------------------------------------

/// The concrete conversion behind a name. The iconv-backed variants name the
/// system character set; the rest are implemented in this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Gb2312,
    Gbk,
    Gb18030,
    Hz,
    Cp932,
    EucJp,
    EucJis2004,
    EucJisx0213,
    ShiftJis,
    ShiftJis2004,
    ShiftJisx0213,
    Cp949,
    EucKr,
    Johab,
    Big5,
    Cp950,
    Big5Hkscs,
    Iso2022Jp,
    Iso2022Jp1,
    Iso2022Jp2,
    Iso2022Jp2004,
    Iso2022Jp3,
    Iso2022JpExt,
    Iso2022Kr,
}

impl Kind {
    /// The spelling CPython uses in registry entries and error messages.
    fn name(self) -> &'static str {
        match self {
            Kind::Gb2312 => "gb2312",
            Kind::Gbk => "gbk",
            Kind::Gb18030 => "gb18030",
            Kind::Hz => "hz",
            Kind::Cp932 => "cp932",
            Kind::EucJp => "euc_jp",
            Kind::EucJis2004 => "euc_jis_2004",
            Kind::EucJisx0213 => "euc_jisx0213",
            Kind::ShiftJis => "shift_jis",
            Kind::ShiftJis2004 => "shift_jis_2004",
            Kind::ShiftJisx0213 => "shift_jisx0213",
            Kind::Cp949 => "cp949",
            Kind::EucKr => "euc_kr",
            Kind::Johab => "johab",
            Kind::Big5 => "big5",
            Kind::Cp950 => "cp950",
            Kind::Big5Hkscs => "big5hkscs",
            Kind::Iso2022Jp => "iso2022_jp",
            Kind::Iso2022Jp1 => "iso2022_jp_1",
            Kind::Iso2022Jp2 => "iso2022_jp_2",
            Kind::Iso2022Jp2004 => "iso2022_jp_2004",
            Kind::Iso2022Jp3 => "iso2022_jp_3",
            Kind::Iso2022JpExt => "iso2022_jp_ext",
            Kind::Iso2022Kr => "iso2022_kr",
        }
    }

    /// The iconv character set that supplies the byte mapping, for the
    /// stateless codecs and for the ISO-2022 sub-conversions.
    fn iconv(self) -> &'static str {
        match self {
            Kind::Gb2312 => "GB2312",
            Kind::Gbk => "GBK",
            Kind::Gb18030 => "GB18030",
            Kind::Cp932 => "CP932",
            Kind::EucJp => "EUC-JP",
            Kind::EucJis2004 | Kind::EucJisx0213 => "EUC-JISX0213",
            Kind::ShiftJis => "SHIFT_JIS",
            Kind::ShiftJis2004 | Kind::ShiftJisx0213 => "SHIFT_JISX0213",
            Kind::Cp949 => "CP949",
            Kind::EucKr => "EUC-KR",
            Kind::Johab => "JOHAB",
            Kind::Big5 => "BIG5",
            Kind::Cp950 => "CP950",
            Kind::Big5Hkscs => "BIG5-HKSCS",
            Kind::Hz => "GB2312",
            _ => "EUC-JP",
        }
    }

    /// Whether the codec's encoder can represent code points above U+FFFF.
    fn allows_supplementary(self) -> bool {
        matches!(self, Kind::Gb18030)
    }

    /// Whether the incremental encoder buffers a trailing JIS X 0213
    /// pair-start character.
    fn pairs_across_calls(self) -> bool {
        matches!(
            self,
            Kind::EucJis2004
                | Kind::EucJisx0213
                | Kind::ShiftJis2004
                | Kind::ShiftJisx0213
                | Kind::Iso2022Jp2004
                | Kind::Iso2022Jp3
        )
    }

    /// Whether this kind uses the ISO-2022 state machine.
    fn is_iso2022(self) -> bool {
        matches!(
            self,
            Kind::Iso2022Jp
                | Kind::Iso2022Jp1
                | Kind::Iso2022Jp2
                | Kind::Iso2022Jp2004
                | Kind::Iso2022Jp3
                | Kind::Iso2022JpExt
                | Kind::Iso2022Kr
        )
    }

    /// Whether this kind is implemented by [`encode_stateless`] /
    /// [`decode_stateless`].
    fn is_stateless(self) -> bool {
        !matches!(
            self,
            Kind::Hz
                | Kind::Iso2022Jp
                | Kind::Iso2022Jp1
                | Kind::Iso2022Jp2
                | Kind::Iso2022Jp2004
                | Kind::Iso2022Jp3
                | Kind::Iso2022JpExt
                | Kind::Iso2022Kr
        )
    }
}

/// One module's registry order, matching CPython's codec list order.
struct CodecEntry {
    name: &'static str,
    kind: Kind,
}

const CN_CODECS: &[CodecEntry] = &[
    CodecEntry {
        name: "gb2312",
        kind: Kind::Gb2312,
    },
    CodecEntry {
        name: "gbk",
        kind: Kind::Gbk,
    },
    CodecEntry {
        name: "gb18030",
        kind: Kind::Gb18030,
    },
    CodecEntry {
        name: "hz",
        kind: Kind::Hz,
    },
];

const JP_CODECS: &[CodecEntry] = &[
    CodecEntry {
        name: "cp932",
        kind: Kind::Cp932,
    },
    CodecEntry {
        name: "euc_jis_2004",
        kind: Kind::EucJis2004,
    },
    CodecEntry {
        name: "euc_jisx0213",
        kind: Kind::EucJisx0213,
    },
    CodecEntry {
        name: "euc_jp",
        kind: Kind::EucJp,
    },
    CodecEntry {
        name: "shift_jis",
        kind: Kind::ShiftJis,
    },
    CodecEntry {
        name: "shift_jis_2004",
        kind: Kind::ShiftJis2004,
    },
    CodecEntry {
        name: "shift_jisx0213",
        kind: Kind::ShiftJisx0213,
    },
];

const KR_CODECS: &[CodecEntry] = &[
    CodecEntry {
        name: "cp949",
        kind: Kind::Cp949,
    },
    CodecEntry {
        name: "euc_kr",
        kind: Kind::EucKr,
    },
    CodecEntry {
        name: "johab",
        kind: Kind::Johab,
    },
];

const TW_CODECS: &[CodecEntry] = &[
    CodecEntry {
        name: "big5",
        kind: Kind::Big5,
    },
    CodecEntry {
        name: "cp950",
        kind: Kind::Cp950,
    },
];

const HK_CODECS: &[CodecEntry] = &[CodecEntry {
    name: "big5hkscs",
    kind: Kind::Big5Hkscs,
}];

const ISO2022_CODECS: &[CodecEntry] = &[
    CodecEntry {
        name: "iso2022_jp",
        kind: Kind::Iso2022Jp,
    },
    CodecEntry {
        name: "iso2022_jp_1",
        kind: Kind::Iso2022Jp1,
    },
    CodecEntry {
        name: "iso2022_jp_2",
        kind: Kind::Iso2022Jp2,
    },
    CodecEntry {
        name: "iso2022_jp_2004",
        kind: Kind::Iso2022Jp2004,
    },
    CodecEntry {
        name: "iso2022_jp_3",
        kind: Kind::Iso2022Jp3,
    },
    CodecEntry {
        name: "iso2022_jp_ext",
        kind: Kind::Iso2022JpExt,
    },
    CodecEntry {
        name: "iso2022_kr",
        kind: Kind::Iso2022Kr,
    },
];

/// The codec list owned by `module`, if this file implements it.
fn module_codecs(module: &str) -> Option<&'static [CodecEntry]> {
    match module {
        "_codecs_cn" => Some(CN_CODECS),
        "_codecs_jp" => Some(JP_CODECS),
        "_codecs_kr" => Some(KR_CODECS),
        "_codecs_tw" => Some(TW_CODECS),
        "_codecs_hk" => Some(HK_CODECS),
        "_codecs_iso2022" => Some(ISO2022_CODECS),
        _ => None,
    }
}

fn kind_named(name: &str) -> Option<Kind> {
    for module in [
        "_codecs_cn",
        "_codecs_jp",
        "_codecs_kr",
        "_codecs_tw",
        "_codecs_hk",
        "_codecs_iso2022",
    ] {
        if let Some(entries) = module_codecs(module) {
            if let Some(entry) = entries.iter().find(|entry| entry.name == name) {
                return Some(entry.kind);
            }
        }
    }
    None
}

// -------------------------------------------------------------------------
// iconv FFI
// -------------------------------------------------------------------------

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
    /// The trailing bytes are a prefix of a character.
    Incomplete,
    /// iconv failed for a reason other than input validity.
    Internal,
}

/// An open iconv descriptor. The descriptor is closed on drop, so an early
/// return or a raised error never leaks it.
struct Converter {
    #[cfg(unix)]
    cd: *mut c_void,
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

    /// Resets the descriptor's shift state.
    fn reset(&mut self) {
        #[cfg(unix)]
        {
            unsafe {
                iconv_ffi::iconv(
                    self.cd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
            }
        }
    }

    /// Converts `input`, appending the produced bytes to `out`. On a fault,
    /// the bytes produced before the fault stay in `out` and the returned
    /// offset locates the offending input byte.
    fn convert_into(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), ConvFault> {
        #[cfg(unix)]
        {
            let mut pos = 0usize;
            let mut chunk = vec![0u8; input.len().saturating_mul(2).saturating_add(64).max(256)];
            loop {
                let mut in_ptr = input[pos..].as_ptr() as *mut c_char;
                let mut in_left = input.len() - pos;
                let mut out_ptr = chunk.as_mut_ptr() as *mut c_char;
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
                    return Err(ConvFault::Incomplete);
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

/// One-shot conversion used by the ISO-2022 designation mappings.
fn convert_oneshot(from: &str, to: &str, input: &[u8]) -> Option<Vec<u8>> {
    let mut converter = Converter::open(from, to)?;
    let mut out = Vec::new();
    converter.convert_into(input, &mut out).ok()?;
    Some(out)
}

/// Whether iconv can convert in both directions for `encoding`.
fn iconv_available(encoding: &str) -> bool {
    Converter::open(encoding, "UTF-8").is_some() && Converter::open("UTF-8", encoding).is_some()
}

/// Whether all character sets a codec needs are present in the system iconv.
fn kind_supported(kind: Kind) -> bool {
    match kind {
        Kind::Hz => iconv_available("GB2312"),
        Kind::Iso2022Jp | Kind::Iso2022Jp1 | Kind::Iso2022JpExt => iconv_available("EUC-JP"),
        Kind::Iso2022Jp2 => {
            iconv_available("EUC-JP")
                && iconv_available("GB2312")
                && iconv_available("EUC-KR")
                && iconv_available("ISO-8859-1")
                && iconv_available("ISO-8859-7")
        }
        Kind::Iso2022Jp2004 | Kind::Iso2022Jp3 => {
            iconv_available("EUC-JP") && iconv_available("EUC-JISX0213")
        }
        Kind::Iso2022Kr => iconv_available("EUC-KR"),
        other => iconv_available(other.iconv()),
    }
}

// -------------------------------------------------------------------------
// Errors shared by every codec
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn lookup_error(message: &str) -> i64 {
    exc::raise_with_message(LOOKUP_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

/// Renders a code point the way CPython's `UnicodeEncodeError` message does.
fn escaped_character(character: char) -> String {
    let code = character as u32;
    if code <= 0xff {
        format!("\\x{code:02x}")
    } else if code <= 0xffff {
        format!("\\u{code:04x}")
    } else {
        format!("\\U{code:08x}")
    }
}

/// The replacement produced by an error handler.
enum Replacement {
    Text(String),
    Bytes(Vec<u8>),
}

/// Resolves and invokes the error handler named `errors` for `error`.
///
/// The handler is looked up through `_codecs.lookup_error`, which honors
/// `codecs.register_error`, so custom handlers see the same exception object
/// as CPython.
fn call_error_handler(errors: &str, error: i64) -> Result<(Replacement, i64), i64> {
    let name = value::str_alloc(errors.to_string());
    let handler = crate::modules::codecs::call("lookup_error", &[name]);
    if exc::pending() {
        return Err(NONE);
    }
    let result = core_builtins::call(handler, 1, [error].as_ptr());
    if exc::pending() {
        return Err(NONE);
    }
    let Some(items) = value::as_tuple(result) else {
        return Err(type_error("error handler must return a tuple"));
    };
    if items.len() != 2 {
        return Err(type_error("error handler must return a tuple"));
    }
    let replacement = if let Some(text) = value::as_str(items[0]) {
        Replacement::Text(text.to_string())
    } else if let Some(bytes) = value::as_bytes(items[0]) {
        Replacement::Bytes(bytes.to_vec())
    } else {
        return Err(type_error("error handler replacement must be str or bytes"));
    };
    let Some(position) = value::as_integer(items[1]) else {
        return Err(type_error("error handler position must be an int"));
    };
    Ok((replacement, position))
}

/// Builds the `UnicodeDecodeError` value CPython hands to a decode handler.
fn decode_exception(kind: Kind, data: &[u8], start: usize, end: usize, reason: &str) -> i64 {
    let name = kind.name();
    let message = if end > start + 1 {
        format!(
            "'{name}' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    } else {
        let byte = data.get(start).copied().unwrap_or(0);
        format!("'{name}' codec can't decode byte 0x{byte:02x} in position {start}: {reason}")
    };
    let args = vec![
        name.to_string(),
        format!("b\"{}\"", String::from_utf8_lossy(data)),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_DECODE_ERROR, &message, args)
}

/// Builds the `UnicodeEncodeError` value CPython hands to an encode handler.
fn encode_exception(kind: Kind, text: &str, start: usize, end: usize, reason: &str) -> i64 {
    let name = kind.name();
    let character = text.chars().nth(start).unwrap_or('\u{0}');
    let message = format!(
        "'{name}' codec can't encode character '{}' in position {start}: {reason}",
        escaped_character(character)
    );
    let args = vec![
        name.to_string(),
        text.to_string(),
        start.to_string(),
        end.to_string(),
        reason.to_string(),
    ];
    exc::exception_value(UNICODE_ENCODE_ERROR, &message, args)
}

/// Byte offsets of each character in `text`, followed by `text.len()`.
fn char_offsets(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect()
}

/// Character index containing `byte`, clamped to the text length.
fn char_index_at(offsets: &[usize], byte: usize) -> usize {
    match offsets.binary_search(&byte) {
        Ok(index) => index,
        Err(index) => index,
    }
    .min(offsets.len() - 1)
}

/// Reports a decode error and returns the position to resume at.
fn handle_decode_error(
    kind: Kind,
    data: &[u8],
    start: usize,
    end: usize,
    reason: &str,
    errors: &str,
    out: &mut Vec<u8>,
) -> Result<usize, i64> {
    let error = decode_exception(kind, data, start, end, reason);
    let (replacement, position) = call_error_handler(errors, error)?;
    match replacement {
        Replacement::Text(text) => out.extend_from_slice(text.as_bytes()),
        Replacement::Bytes(_) => {
            return Err(type_error(
                "decoding error handler must return (str, int) tuple",
            ))
        }
    }
    let mut next = position;
    if next < 0 {
        next += data.len() as i64;
    }
    if next < 0 || next > data.len() as i64 {
        return Err(exc::raise_with_message(
            INDEX_ERROR,
            &format!("position {position} from error handler out of bounds"),
        ));
    }
    let next = next as usize;
    // A handler that does not advance would loop forever on the same bytes;
    // consume the error span instead (CPython relies on handlers advancing).
    Ok(if next <= start {
        end.max(start + 1)
    } else {
        next
    })
}

/// Re-encodes a handler-provided replacement under the codec's strict handler.
type EncodeReplacement<'a> = &'a mut dyn FnMut(&str, &mut Vec<u8>) -> Result<(), i64>;

/// Reports an encode error and returns the next character index.
#[allow(clippy::too_many_arguments)]
fn handle_encode_error(
    kind: Kind,
    text: &str,
    offsets: &[usize],
    start: usize,
    end: usize,
    reason: &str,
    errors: &str,
    out: &mut Vec<u8>,
    encode_replacement: EncodeReplacement<'_>,
) -> Result<usize, i64> {
    let chars = offsets.len() - 1;
    let error = encode_exception(kind, text, start, end, reason);
    let (replacement, position) = call_error_handler(errors, error)?;
    let mut next = position;
    if next < 0 {
        next += chars as i64;
    }
    if next < 0 || next > chars as i64 {
        return Err(exc::raise_with_message(
            INDEX_ERROR,
            &format!("position {position} from error handler out of bounds"),
        ));
    }
    let next = next as usize;
    match replacement {
        Replacement::Bytes(bytes) => out.extend_from_slice(&bytes),
        Replacement::Text(text_replacement) => {
            if !text_replacement.is_empty() {
                // CPython re-encodes the replacement under the strict handler
                // and the codec's *current* shift state; an unencodable
                // replacement propagates its exception.
                encode_replacement(&text_replacement, out)?;
            }
        }
    }
    Ok(if next <= start {
        end.max(start + 1)
    } else {
        next
    })
}

// -------------------------------------------------------------------------
// Stateless codecs (iconv-backed)
// -------------------------------------------------------------------------

/// A character that is not handled by the codec's iconv table.
enum Special {
    /// The exact bytes CPython's encoder emits for this character.
    Bytes(Vec<u8>),
    /// The codec cannot represent this character at all.
    Unmappable,
}

/// Finds the next character that must bypass iconv.
fn next_special(kind: Kind, text: &str, from_byte: usize) -> Option<(usize, char, Special)> {
    for (offset, character) in text[from_byte..].char_indices() {
        let byte = from_byte + offset;
        let code = character as u32;
        if !kind.allows_supplementary() && code > 0xFFFF {
            return Some((byte, character, Special::Unmappable));
        }
        if kind == Kind::Cp932 {
            match code {
                0x80 => return Some((byte, character, Special::Bytes(vec![0x80]))),
                0xF8F0 => return Some((byte, character, Special::Bytes(vec![0xA0]))),
                0xF8F1..=0xF8F3 => {
                    return Some((
                        byte,
                        character,
                        Special::Bytes(vec![(code - 0xF8F1 + 0xFD) as u8]),
                    ))
                }
                0xE000..=0xE757 => {
                    let value = code - 0xE000;
                    let first = 0xF0 + (value / 188) as u8;
                    let second = value % 188;
                    let second = if second < 0x3F {
                        second as u8 + 0x40
                    } else {
                        second as u8 + 0x41
                    };
                    return Some((byte, character, Special::Bytes(vec![first, second])));
                }
                _ => {}
            }
        }
    }
    None
}

/// Converts `text[pos_byte..end_byte]` through iconv, handling per-character
/// errors. Returns the position to resume at (equal to `end_byte` on success).
#[allow(clippy::too_many_arguments)]
fn encode_span(
    converter: &mut Converter,
    kind: Kind,
    text: &str,
    offsets: &[usize],
    errors: &str,
    out: &mut Vec<u8>,
    encode_replacement: EncodeReplacement<'_>,
    mut pos_byte: usize,
    end_byte: usize,
) -> Result<usize, i64> {
    let bytes = text.as_bytes();
    while pos_byte < end_byte {
        let mut produced = Vec::new();
        match converter.convert_into(&bytes[pos_byte..end_byte], &mut produced) {
            Ok(()) => {
                out.extend_from_slice(&produced);
                return Ok(end_byte);
            }
            Err(ConvFault::Illegal(offset)) => {
                out.extend_from_slice(&produced);
                let absolute = pos_byte + offset;
                let start = char_index_at(offsets, absolute);
                let end = (start + 1).min(offsets.len() - 1);
                let next = handle_encode_error(
                    kind,
                    text,
                    offsets,
                    start,
                    end,
                    "illegal multibyte sequence",
                    errors,
                    out,
                    encode_replacement,
                )?;
                converter.reset();
                pos_byte = offsets[next];
            }
            Err(ConvFault::Incomplete) => {
                out.extend_from_slice(&produced);
                let start = char_index_at(offsets, pos_byte).min(offsets.len().saturating_sub(2));
                let end = (start + 1).min(offsets.len() - 1);
                let next = handle_encode_error(
                    kind,
                    text,
                    offsets,
                    start,
                    end,
                    "incomplete multibyte sequence",
                    errors,
                    out,
                    encode_replacement,
                )?;
                converter.reset();
                pos_byte = offsets[next];
            }
            Err(ConvFault::Internal) => {
                out.extend_from_slice(&produced);
                return Err(runtime_error("internal codec error"));
            }
        }
    }
    Ok(end_byte)
}

/// Encodes `text` with a stateless iconv-backed codec.
fn encode_stateless(kind: Kind, text: &str, errors: &str) -> Result<Vec<u8>, i64> {
    let Some(mut converter) = Converter::open("UTF-8", kind.iconv()) else {
        return Err(runtime_error("codec is not available"));
    };
    let offsets = char_offsets(text);
    let mut out = Vec::with_capacity(text.len());
    let mut encode_replacement = |replacement: &str, target: &mut Vec<u8>| -> Result<(), i64> {
        let bytes = encode_stateless(kind, replacement, "strict")?;
        target.extend_from_slice(&bytes);
        Ok(())
    };
    let mut pos_byte = 0usize;
    while pos_byte < text.len() {
        match next_special(kind, text, pos_byte) {
            Some((special_byte, character, action)) => {
                pos_byte = encode_span(
                    &mut converter,
                    kind,
                    text,
                    &offsets,
                    errors,
                    &mut out,
                    &mut encode_replacement,
                    pos_byte,
                    special_byte,
                )?;
                if pos_byte != special_byte {
                    continue;
                }
                match action {
                    Special::Bytes(bytes) => {
                        out.extend_from_slice(&bytes);
                        pos_byte += character.len_utf8();
                    }
                    Special::Unmappable => {
                        let start = char_index_at(&offsets, pos_byte);
                        let next = handle_encode_error(
                            kind,
                            text,
                            &offsets,
                            start,
                            start + 1,
                            "illegal multibyte sequence",
                            errors,
                            &mut out,
                            &mut encode_replacement,
                        )?;
                        pos_byte = offsets[next];
                    }
                }
            }
            None => {
                pos_byte = encode_span(
                    &mut converter,
                    kind,
                    text,
                    &offsets,
                    errors,
                    &mut out,
                    &mut encode_replacement,
                    pos_byte,
                    text.len(),
                )?;
            }
        }
    }
    Ok(out)
}

fn push_char(out: &mut Vec<u8>, character: char) {
    let mut buffer = [0u8; 4];
    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

/// How one decoded unit is produced.
enum Plan {
    /// Already UTF-8 bytes and the number of input bytes they consume.
    Direct(Vec<u8>, usize),
    /// `iconv` converts this many input bytes.
    Convert(usize),
    Illegal,
    Incomplete,
}

fn plan_codec(kind: Kind, data: &[u8], pos: usize) -> Plan {
    let byte = data[pos];
    let rest = &data[pos..];
    match kind {
        Kind::Gb2312 => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0xA1..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(2)
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Gbk => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0x81..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFE).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Gb18030 => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0x81..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x30..=0x39).contains(&rest[1]) {
                    if rest.len() < 4 {
                        Plan::Incomplete
                    } else if (0x81..=0xFE).contains(&rest[2]) && (0x30..=0x39).contains(&rest[3]) {
                        Plan::Convert(4)
                    } else {
                        Plan::Illegal
                    }
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFE).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Big5 | Kind::Cp950 | Kind::Big5Hkscs => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0x81..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(2)
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::EucJp | Kind::EucJis2004 | Kind::EucJisx0213 => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if byte == 0x8E {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0xA1..=0xDF).contains(&rest[1]) {
                    let mut bytes = Vec::new();
                    push_char(
                        &mut bytes,
                        char::from_u32(0xFEC0 + rest[1] as u32).unwrap_or('\u{FFFD}'),
                    );
                    Plan::Direct(bytes, 2)
                } else {
                    Plan::Illegal
                }
            } else if byte == 0x8F {
                if rest.len() < 3 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(3)
                }
            } else if (0xA1..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(2)
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::ShiftJis => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0xA1..=0xDF).contains(&byte) {
                let mut bytes = Vec::new();
                push_char(
                    &mut bytes,
                    char::from_u32(0xFEC0 + byte as u32).unwrap_or('\u{FFFD}'),
                );
                Plan::Direct(bytes, 1)
            } else if (0x81..=0x9F).contains(&byte) || (0xE0..=0xEA).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFC).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::ShiftJis2004 | Kind::ShiftJisx0213 => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0xA1..=0xDF).contains(&byte) {
                let mut bytes = Vec::new();
                push_char(
                    &mut bytes,
                    char::from_u32(0xFEC0 + byte as u32).unwrap_or('\u{FFFD}'),
                );
                Plan::Direct(bytes, 1)
            } else if (0x81..=0x9F).contains(&byte) || (0xE0..=0xFC).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFC).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Cp932 => {
            if byte == 0x80 {
                let mut bytes = Vec::new();
                push_char(&mut bytes, '\u{80}');
                Plan::Direct(bytes, 1)
            } else if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0xA1..=0xDF).contains(&byte) {
                let decoded = if byte == 0xA0 {
                    0xF8F0
                } else {
                    0xFEC0 + byte as u32
                };
                let mut bytes = Vec::new();
                push_char(&mut bytes, char::from_u32(decoded).unwrap_or('\u{FFFD}'));
                Plan::Direct(bytes, 1)
            } else if (0xFD..=0xFF).contains(&byte) {
                let decoded = 0xF8F1 + (byte as u32 - 0xFD);
                let mut bytes = Vec::new();
                push_char(&mut bytes, char::from_u32(decoded).unwrap_or('\u{FFFD}'));
                Plan::Direct(bytes, 1)
            } else if (0x81..=0x9F).contains(&byte) || (0xE0..=0xEA).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFC).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else if (0xF0..=0xF9).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFC).contains(&rest[1]) {
                    let offset = if rest[1] < 0x80 {
                        rest[1] as u32 - 0x40
                    } else {
                        rest[1] as u32 - 0x41
                    };
                    let decoded = 0xE000 + 188 * (byte as u32 - 0xF0) + offset;
                    let mut bytes = Vec::new();
                    push_char(&mut bytes, char::from_u32(decoded).unwrap_or('\u{FFFD}'));
                    Plan::Direct(bytes, 2)
                } else {
                    Plan::Illegal
                }
            } else if (0xEB..=0xEF).contains(&byte) || (0xFA..=0xFC).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x40..=0x7E).contains(&rest[1]) || (0x80..=0xFC).contains(&rest[1]) {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::EucKr => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0xA1..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(2)
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Cp949 => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0x81..=0xFE).contains(&byte) {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else if (0x41..=0x5A).contains(&rest[1])
                    || (0x61..=0x7A).contains(&rest[1])
                    || (0x81..=0xFE).contains(&rest[1])
                {
                    Plan::Convert(2)
                } else {
                    Plan::Illegal
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Johab => {
            if byte < 0x80 {
                Plan::Direct(vec![byte], 1)
            } else if (0x84..=0xF9).contains(&byte) && byte != 0xDF {
                if rest.len() < 2 {
                    Plan::Incomplete
                } else {
                    Plan::Convert(2)
                }
            } else {
                Plan::Illegal
            }
        }
        Kind::Hz
        | Kind::Iso2022Jp
        | Kind::Iso2022Jp1
        | Kind::Iso2022Jp2
        | Kind::Iso2022Jp2004
        | Kind::Iso2022Jp3
        | Kind::Iso2022JpExt
        | Kind::Iso2022Kr => Plan::Illegal,
    }
}

/// Decodes `data` with a stateless iconv-backed codec. Returns the number of
/// bytes consumed; with `final_ == false` a trailing partial sequence is left
/// unconsumed for the caller to buffer.
fn decode_stateless(
    kind: Kind,
    data: &[u8],
    final_: bool,
    errors: &str,
    out: &mut Vec<u8>,
) -> Result<usize, i64> {
    let Some(mut converter) = Converter::open(kind.iconv(), "UTF-8") else {
        return Err(runtime_error("codec is not available"));
    };
    let mut pos = 0usize;
    while pos < data.len() {
        match plan_codec(kind, data, pos) {
            Plan::Direct(bytes, width) => {
                out.extend_from_slice(&bytes);
                pos += width;
            }
            Plan::Convert(width) => {
                let mut produced = Vec::new();
                match converter.convert_into(&data[pos..pos + width], &mut produced) {
                    Ok(()) => {
                        out.extend_from_slice(&produced);
                        pos += width;
                    }
                    Err(ConvFault::Illegal(_)) => {
                        converter.reset();
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            pos + 1,
                            "illegal multibyte sequence",
                            errors,
                            out,
                        )?;
                    }
                    Err(ConvFault::Internal) => {
                        return Err(runtime_error("internal codec error"));
                    }
                    Err(ConvFault::Incomplete) => {
                        converter.reset();
                        if !final_ {
                            return Ok(pos);
                        }
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            data.len(),
                            "incomplete multibyte sequence",
                            errors,
                            out,
                        )?;
                    }
                }
            }
            Plan::Illegal => {
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    pos + 1,
                    "illegal multibyte sequence",
                    errors,
                    out,
                )?;
            }
            Plan::Incomplete => {
                if !final_ {
                    return Ok(pos);
                }
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    data.len(),
                    "incomplete multibyte sequence",
                    errors,
                    out,
                )?;
            }
        }
    }
    Ok(pos)
}

// -------------------------------------------------------------------------
// HZ
// -------------------------------------------------------------------------

/// HZ switches between ASCII and GB2312 with `~{`, `~}`, `~~` and `~\n`.
struct HzState {
    gb: bool,
}

impl HzState {
    fn new() -> Self {
        HzState { gb: false }
    }
}

fn hz_decode(
    state: &mut HzState,
    data: &[u8],
    final_: bool,
    errors: &str,
    out: &mut Vec<u8>,
) -> Result<usize, i64> {
    let kind = Kind::Hz;
    let Some(mut converter) = Converter::open("GB2312", "UTF-8") else {
        return Err(runtime_error("codec is not available"));
    };
    let mut pos = 0usize;
    while pos < data.len() {
        let byte = data[pos];
        if !state.gb {
            if byte < 0x80 {
                if byte == b'~' {
                    if pos + 1 >= data.len() {
                        if !final_ {
                            return Ok(pos);
                        }
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            data.len(),
                            "incomplete multibyte sequence",
                            errors,
                            out,
                        )?;
                        continue;
                    }
                    match data[pos + 1] {
                        b'~' => {
                            out.push(b'~');
                            pos += 2;
                        }
                        b'{' => {
                            state.gb = true;
                            pos += 2;
                        }
                        b'\n' => pos += 2,
                        _ => {
                            pos = handle_decode_error(
                                kind,
                                data,
                                pos,
                                pos + 1,
                                "illegal multibyte sequence",
                                errors,
                                out,
                            )?;
                        }
                    }
                } else {
                    out.push(byte);
                    pos += 1;
                }
            } else {
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    pos + 1,
                    "illegal multibyte sequence",
                    errors,
                    out,
                )?;
            }
        } else if byte == b'~' {
            if pos + 1 >= data.len() {
                if !final_ {
                    return Ok(pos);
                }
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    data.len(),
                    "incomplete multibyte sequence",
                    errors,
                    out,
                )?;
                continue;
            }
            match data[pos + 1] {
                b'}' => {
                    state.gb = false;
                    pos += 2;
                }
                b'\n' => pos += 2,
                _ => {
                    pos = handle_decode_error(
                        kind,
                        data,
                        pos,
                        pos + 1,
                        "illegal multibyte sequence",
                        errors,
                        out,
                    )?;
                }
            }
        } else if (0x21..=0x7E).contains(&byte) {
            if pos + 1 >= data.len() {
                if !final_ {
                    return Ok(pos);
                }
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    data.len(),
                    "incomplete multibyte sequence",
                    errors,
                    out,
                )?;
                continue;
            }
            let second = data[pos + 1];
            if !(0x21..=0x7E).contains(&second) {
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    pos + 1,
                    "illegal multibyte sequence",
                    errors,
                    out,
                )?;
                continue;
            }
            let pair = [byte | 0x80, second | 0x80];
            let mut produced = Vec::new();
            match converter.convert_into(&pair, &mut produced) {
                Ok(()) => {
                    out.extend_from_slice(&produced);
                    pos += 2;
                }
                Err(_) => {
                    converter.reset();
                    pos = handle_decode_error(
                        kind,
                        data,
                        pos,
                        pos + 1,
                        "illegal multibyte sequence",
                        errors,
                        out,
                    )?;
                }
            }
        } else {
            pos = handle_decode_error(
                kind,
                data,
                pos,
                pos + 1,
                "illegal multibyte sequence",
                errors,
                out,
            )?;
        }
    }
    Ok(pos)
}

fn hz_encode(state: &mut HzState, text: &str, final_: bool, errors: &str) -> Result<Vec<u8>, i64> {
    let kind = Kind::Hz;
    let offsets = char_offsets(text);
    let chars = offsets.len() - 1;
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < chars {
        let character = text[offsets[index]..].chars().next().unwrap_or('\u{0}');
        if (character as u32) < 0x80 {
            if state.gb {
                out.extend_from_slice(b"~}");
                state.gb = false;
            }
            if character == '~' {
                out.extend_from_slice(b"~~");
            } else {
                out.push(character as u8);
            }
            index += 1;
            continue;
        }
        let encoded = convert_oneshot("UTF-8", "GB2312", character.to_string().as_bytes());
        let valid = encoded.as_ref().is_some_and(|bytes| {
            bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b))
        });
        if !valid {
            let next = {
                let mut encode_replacement =
                    |replacement: &str, target: &mut Vec<u8>| -> Result<(), i64> {
                        let bytes = hz_encode(&mut *state, replacement, false, "strict")?;
                        target.extend_from_slice(&bytes);
                        Ok(())
                    };
                handle_encode_error(
                    kind,
                    text,
                    &offsets,
                    index,
                    index + 1,
                    "illegal multibyte sequence",
                    errors,
                    &mut out,
                    &mut encode_replacement,
                )?
            };
            index = next;
            continue;
        }
        let bytes = encoded.unwrap_or_default();
        if !state.gb {
            out.extend_from_slice(b"~{");
            state.gb = true;
        }
        out.push(bytes[0] - 0x80);
        out.push(bytes[1] - 0x80);
        index += 1;
    }
    if final_ && state.gb {
        out.extend_from_slice(b"~}");
        state.gb = false;
    }
    Ok(out)
}

// -------------------------------------------------------------------------
// ISO-2022
// -------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cs {
    None,
    Ascii,
    Roman,
    Katakana,
    Jis0208,
    Jis0212,
    Jis0213_2000,
    Jis0213_2004,
    Jis0213_2,
    Gb2312,
    Ksx1001,
    Iso8859_1,
    Iso8859_7,
}

impl Cs {
    fn mark(self) -> u8 {
        match self {
            Cs::None => 0,
            Cs::Ascii => b'B',
            Cs::Roman => b'J',
            Cs::Katakana => b'I',
            Cs::Jis0208 => b'B',
            Cs::Jis0212 => b'D',
            Cs::Jis0213_2000 => b'O',
            Cs::Jis0213_2004 => b'Q',
            Cs::Jis0213_2 => b'P',
            Cs::Gb2312 => b'A',
            Cs::Ksx1001 => b'C',
            Cs::Iso8859_1 => b'A',
            Cs::Iso8859_7 => b'F',
        }
    }

    fn dbcs(self) -> bool {
        matches!(
            self,
            Cs::Jis0208
                | Cs::Jis0212
                | Cs::Jis0213_2000
                | Cs::Jis0213_2004
                | Cs::Jis0213_2
                | Cs::Gb2312
                | Cs::Ksx1001
        )
    }

    fn width(self) -> usize {
        if self.dbcs() {
            2
        } else {
            1
        }
    }

    fn from_byte(byte: u8) -> Cs {
        match byte & 0x7f {
            b'B' if byte & 0x80 != 0 => Cs::Jis0208,
            b'D' if byte & 0x80 != 0 => Cs::Jis0212,
            b'O' if byte & 0x80 != 0 => Cs::Jis0213_2000,
            b'Q' if byte & 0x80 != 0 => Cs::Jis0213_2004,
            b'P' if byte & 0x80 != 0 => Cs::Jis0213_2,
            b'A' if byte & 0x80 != 0 => Cs::Gb2312,
            b'C' if byte & 0x80 != 0 => Cs::Ksx1001,
            b'B' => Cs::Ascii,
            b'J' => Cs::Roman,
            b'I' => Cs::Katakana,
            b'A' => Cs::Iso8859_1,
            b'F' => Cs::Iso8859_7,
            _ => Cs::None,
        }
    }

    fn from_mark(mark: u8, dbcs: bool) -> Cs {
        if dbcs {
            match mark {
                b'B' | b'@' => Cs::Jis0208,
                b'D' => Cs::Jis0212,
                b'O' => Cs::Jis0213_2000,
                b'Q' => Cs::Jis0213_2004,
                b'P' => Cs::Jis0213_2,
                b'A' => Cs::Gb2312,
                b'C' => Cs::Ksx1001,
                _ => Cs::None,
            }
        } else {
            match mark {
                b'B' => Cs::Ascii,
                b'J' => Cs::Roman,
                b'I' => Cs::Katakana,
                b'A' => Cs::Iso8859_1,
                b'F' => Cs::Iso8859_7,
                _ => Cs::None,
            }
        }
    }
}

struct IsoConfig {
    /// Character sets the decoder accepts and the encoder may use.
    allowed: &'static [(Cs, u8)],
    /// Encoder trial order, matching `_codecs_iso2022.c`'s designation lists.
    order: &'static [Cs],
    no_shift: bool,
    use_g2: bool,
}

impl IsoConfig {
    fn plane_of(&self, cs: Cs) -> Option<u8> {
        self.allowed
            .iter()
            .find(|(candidate, _)| *candidate == cs)
            .map(|(_, plane)| *plane)
    }

    fn allows(&self, cs: Cs) -> bool {
        // CPython always accepts the ASCII designation; the per-codec list
        // only restricts the non-ASCII character sets.
        cs == Cs::Ascii
            || (cs != Cs::None && self.allowed.iter().any(|(candidate, _)| *candidate == cs))
    }

    fn has_jis0213(&self) -> bool {
        self.allowed
            .iter()
            .any(|(cs, _)| matches!(cs, Cs::Jis0213_2000 | Cs::Jis0213_2004 | Cs::Jis0213_2))
    }
}

const JP_ALLOWED: &[(Cs, u8)] = &[(Cs::Jis0208, 0), (Cs::Roman, 0)];
const JP_ORDER: &[Cs] = &[Cs::Jis0208, Cs::Roman];
const JP1_ALLOWED: &[(Cs, u8)] = &[(Cs::Jis0208, 0), (Cs::Jis0212, 0), (Cs::Roman, 0)];
const JP1_ORDER: &[Cs] = &[Cs::Jis0208, Cs::Jis0212, Cs::Roman];
const JP2_ALLOWED: &[(Cs, u8)] = &[
    (Cs::Jis0208, 0),
    (Cs::Jis0212, 0),
    (Cs::Ksx1001, 0),
    (Cs::Gb2312, 0),
    (Cs::Roman, 0),
    (Cs::Iso8859_1, 2),
    (Cs::Iso8859_7, 2),
];
const JP2_ORDER: &[Cs] = &[
    Cs::Jis0208,
    Cs::Jis0212,
    Cs::Ksx1001,
    Cs::Gb2312,
    Cs::Roman,
    Cs::Iso8859_1,
    Cs::Iso8859_7,
];
const JP3_ALLOWED: &[(Cs, u8)] = &[(Cs::Jis0213_2000, 0), (Cs::Jis0208, 0), (Cs::Jis0213_2, 0)];
const JP3_ORDER: &[Cs] = &[Cs::Jis0213_2000, Cs::Jis0208, Cs::Jis0213_2];
const JP2004_ALLOWED: &[(Cs, u8)] = &[(Cs::Jis0213_2004, 0), (Cs::Jis0208, 0), (Cs::Jis0213_2, 0)];
const JP2004_ORDER: &[Cs] = &[Cs::Jis0213_2004, Cs::Jis0208, Cs::Jis0213_2];
const JPEXT_ALLOWED: &[(Cs, u8)] = &[
    (Cs::Jis0208, 0),
    (Cs::Jis0212, 0),
    (Cs::Roman, 0),
    (Cs::Katakana, 0),
];
const JPEXT_ORDER: &[Cs] = &[Cs::Jis0208, Cs::Jis0212, Cs::Roman, Cs::Katakana];
const KR_ALLOWED: &[(Cs, u8)] = &[(Cs::Ksx1001, 1)];
const KR_ORDER: &[Cs] = &[Cs::Ksx1001];

static CONFIG_JP: IsoConfig = IsoConfig {
    allowed: JP_ALLOWED,
    order: JP_ORDER,
    no_shift: true,
    use_g2: false,
};
static CONFIG_JP1: IsoConfig = IsoConfig {
    allowed: JP1_ALLOWED,
    order: JP1_ORDER,
    no_shift: true,
    use_g2: false,
};
static CONFIG_JP2: IsoConfig = IsoConfig {
    allowed: JP2_ALLOWED,
    order: JP2_ORDER,
    no_shift: true,
    use_g2: true,
};
static CONFIG_JP3: IsoConfig = IsoConfig {
    allowed: JP3_ALLOWED,
    order: JP3_ORDER,
    no_shift: true,
    use_g2: false,
};
static CONFIG_JP2004: IsoConfig = IsoConfig {
    allowed: JP2004_ALLOWED,
    order: JP2004_ORDER,
    no_shift: true,
    use_g2: false,
};
static CONFIG_JPEXT: IsoConfig = IsoConfig {
    allowed: JPEXT_ALLOWED,
    order: JPEXT_ORDER,
    no_shift: true,
    use_g2: false,
};
static CONFIG_KR: IsoConfig = IsoConfig {
    allowed: KR_ALLOWED,
    order: KR_ORDER,
    no_shift: false,
    use_g2: false,
};

fn iso_config(kind: Kind) -> &'static IsoConfig {
    match kind {
        Kind::Iso2022Jp => &CONFIG_JP,
        Kind::Iso2022Jp1 => &CONFIG_JP1,
        Kind::Iso2022Jp2 => &CONFIG_JP2,
        Kind::Iso2022Jp2004 => &CONFIG_JP2004,
        Kind::Iso2022Jp3 => &CONFIG_JP3,
        Kind::Iso2022JpExt => &CONFIG_JPEXT,
        Kind::Iso2022Kr => &CONFIG_KR,
        _ => &CONFIG_JP,
    }
}

/// ISO-2022 G0-G3 designations plus the locking-shift/escape-throughout bits.
#[derive(Clone, Copy)]
struct IsoState {
    g: [Cs; 4],
    shifted: bool,
    esc_through: bool,
}

impl IsoState {
    fn encoder_start() -> Self {
        IsoState {
            g: [Cs::Ascii, Cs::Ascii, Cs::None, Cs::None],
            shifted: false,
            esc_through: false,
        }
    }

    fn decoder_start() -> Self {
        IsoState {
            g: [Cs::Ascii, Cs::Ascii, Cs::Ascii, Cs::None],
            shifted: false,
            esc_through: false,
        }
    }

    fn raw_bytes(self) -> [u8; 8] {
        let mut bytes = [0u8; 8];
        for (index, cs) in self.g.iter().enumerate() {
            bytes[index] = cs.mark() | if cs.dbcs() { 0x80 } else { 0 };
        }
        bytes[4] = u8::from(self.shifted) | (u8::from(self.esc_through) << 1);
        bytes
    }

    /// Raw little-endian integer (not yet tagged); [`store_state_int`] tags it.
    fn to_value(self) -> i64 {
        i64::from_le_bytes(self.raw_bytes())
    }

    fn from_value(value: i64) -> Option<IsoState> {
        if value < 0 {
            return None;
        }
        let bytes = (value as u64).to_le_bytes();
        let mut g = [Cs::None; 4];
        for index in 0..4 {
            g[index] = Cs::from_byte(bytes[index]);
        }
        Some(IsoState {
            g,
            shifted: bytes[4] & 1 != 0,
            esc_through: bytes[4] & 2 != 0,
        })
    }
}

fn is_esc_end(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte == b'@'
}

fn is_iso2022_esc(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'$' | b'.' | b'&')
}

enum EscFault {
    Incomplete,
    Illegal(usize),
}

fn parse_escape(config: &IsoConfig, data: &[u8]) -> Result<(Cs, u8, usize), EscFault> {
    let mut length = 0usize;
    for (index, byte) in data.iter().enumerate().take(16).skip(1) {
        if is_esc_end(*byte) {
            length = index + 1;
            break;
        }
    }
    if length == 0 {
        if data.len() < 16 {
            return Err(EscFault::Incomplete);
        }
        return Err(EscFault::Illegal(1));
    }
    let designator = |mark: u8, dbcs: bool| -> Result<Cs, EscFault> {
        let cs = Cs::from_mark(mark, dbcs);
        if config.allows(cs) {
            Ok(cs)
        } else {
            Err(EscFault::Illegal(length))
        }
    };
    match length {
        3 => {
            if data[1] == b'$' {
                let cs = designator(data[2], true)?;
                Ok((cs, 0, length))
            } else {
                let plane = match data[1] {
                    b'(' => 0,
                    b')' => 1,
                    b'.' => 2,
                    _ => return Err(EscFault::Illegal(length)),
                };
                let cs = designator(data[2], false)?;
                Ok((cs, plane, length))
            }
        }
        4 => {
            if data[1] != b'$' || (data[2] != b'(' && data[2] != b')') {
                return Err(EscFault::Illegal(length));
            }
            let plane = if data[2] == b'(' { 0 } else { 1 };
            let cs = designator(data[3], true)?;
            Ok((cs, plane, length))
        }
        _ => Err(EscFault::Illegal(length)),
    }
}

/// Decodes an ISO-2022 stream. Returns the consumed byte count.
fn iso_decode(
    kind: Kind,
    config: &IsoConfig,
    state: &mut IsoState,
    data: &[u8],
    final_: bool,
    errors: &str,
    out: &mut Vec<u8>,
) -> Result<usize, i64> {
    let mut pos = 0usize;
    while pos < data.len() {
        let byte = data[pos];
        if state.esc_through {
            push_char(out, char::from_u32(byte as u32).unwrap_or('\u{FFFD}'));
            pos += 1;
            if is_esc_end(byte) {
                state.esc_through = false;
            }
            continue;
        }
        match byte {
            0x1B => {
                if pos + 1 >= data.len() {
                    if !final_ {
                        return Ok(pos);
                    }
                    pos = handle_decode_error(
                        kind,
                        data,
                        pos,
                        data.len(),
                        "incomplete multibyte sequence",
                        errors,
                        out,
                    )?;
                    continue;
                }
                let next = data[pos + 1];
                if is_iso2022_esc(next) {
                    match parse_escape(config, &data[pos..]) {
                        Ok((cs, plane, length)) => {
                            state.g[plane as usize] = cs;
                            pos += length;
                        }
                        Err(EscFault::Incomplete) => {
                            if !final_ {
                                return Ok(pos);
                            }
                            pos = handle_decode_error(
                                kind,
                                data,
                                pos,
                                data.len(),
                                "incomplete multibyte sequence",
                                errors,
                                out,
                            )?;
                        }
                        Err(EscFault::Illegal(length)) => {
                            let end = (pos + length).min(data.len());
                            pos = handle_decode_error(
                                kind,
                                data,
                                pos,
                                end,
                                "illegal multibyte sequence",
                                errors,
                                out,
                            )?;
                        }
                    }
                } else if config.use_g2 && next == b'N' {
                    if pos + 2 >= data.len() {
                        if !final_ {
                            return Ok(pos);
                        }
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            data.len(),
                            "incomplete multibyte sequence",
                            errors,
                            out,
                        )?;
                        continue;
                    }
                    let decoded = match state.g[2] {
                        Cs::Ascii if data[pos + 2] < 0x80 => Some(data[pos + 2] as u32),
                        Cs::Iso8859_1 if data[pos + 2] < 0x80 => Some(0x80 + data[pos + 2] as u32),
                        Cs::Iso8859_7 => {
                            convert_oneshot("ISO-8859-7", "UTF-8", &[data[pos + 2] ^ 0x80])
                                .and_then(|bytes| {
                                    std::str::from_utf8(&bytes)
                                        .ok()
                                        .and_then(|text| text.chars().next())
                                        .map(|character| character as u32)
                                })
                        }
                        _ => None,
                    };
                    match decoded {
                        Some(character) => {
                            push_char(out, char::from_u32(character).unwrap_or('\u{FFFD}'));
                            pos += 3;
                        }
                        None => {
                            let end = (pos + 3).min(data.len());
                            pos = handle_decode_error(
                                kind,
                                data,
                                pos,
                                end,
                                "illegal multibyte sequence",
                                errors,
                                out,
                            )?;
                        }
                    }
                } else {
                    out.push(0x1B);
                    state.esc_through = true;
                    pos += 1;
                }
            }
            0x0F | 0x0E => {
                if config.no_shift {
                    out.push(byte);
                } else {
                    state.shifted = byte == 0x0E;
                }
                pos += 1;
            }
            0x0A => {
                state.shifted = false;
                out.push(b'\n');
                pos += 1;
            }
            byte if byte < 0x20 => {
                out.push(byte);
                pos += 1;
            }
            byte if byte >= 0x80 => {
                pos = handle_decode_error(
                    kind,
                    data,
                    pos,
                    pos + 1,
                    "illegal multibyte sequence",
                    errors,
                    out,
                )?;
            }
            _ => {
                let cs = if state.shifted {
                    state.g[1]
                } else {
                    state.g[0]
                };
                if cs == Cs::Ascii || cs == Cs::None {
                    if cs == Cs::None {
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            pos + 1,
                            "illegal multibyte sequence",
                            errors,
                            out,
                        )?;
                    } else {
                        out.push(byte);
                        pos += 1;
                    }
                    continue;
                }
                let width = cs.width();
                if data.len() - pos < width {
                    if !final_ {
                        return Ok(pos);
                    }
                    pos = handle_decode_error(
                        kind,
                        data,
                        pos,
                        data.len(),
                        "incomplete multibyte sequence",
                        errors,
                        out,
                    )?;
                    continue;
                }
                let decoded = decode_designation(cs, &data[pos..pos + width]);
                match decoded {
                    Some(bytes) => {
                        out.extend_from_slice(&bytes);
                        pos += width;
                    }
                    None => {
                        pos = handle_decode_error(
                            kind,
                            data,
                            pos,
                            pos + width,
                            "illegal multibyte sequence",
                            errors,
                            out,
                        )?;
                    }
                }
            }
        }
    }
    Ok(pos)
}

/// Decodes one character of a designated ISO-2022 character set.
fn decode_designation(cs: Cs, bytes: &[u8]) -> Option<Vec<u8>> {
    let byte = bytes[0];
    match cs {
        Cs::Roman => {
            let character = match byte {
                0x00..=0x5B => byte as u32,
                0x5C => 0x00A5,
                0x5D..=0x7D => byte as u32,
                0x7E => 0x203E,
                0x7F => 0x7F,
                _ => return None,
            };
            let mut out = Vec::new();
            push_char(&mut out, char::from_u32(character)?);
            Some(out)
        }
        Cs::Katakana => {
            // ISO-2022 designates JIS X 0201 katakana in the 7-bit GL area.
            let raw = byte ^ 0x80;
            if !(0xA1..=0xDF).contains(&raw) {
                return None;
            }
            let mut out = Vec::new();
            push_char(&mut out, char::from_u32(0xFEC0 + raw as u32)?);
            Some(out)
        }
        Cs::Iso8859_1 => {
            let mut out = Vec::new();
            push_char(&mut out, char::from_u32(byte as u32)?);
            Some(out)
        }
        Cs::Iso8859_7 => convert_oneshot("ISO-8859-7", "UTF-8", &[byte]),
        Cs::Jis0208 => {
            let pair = [bytes[0] | 0x80, bytes[1] | 0x80];
            convert_oneshot("EUC-JP", "UTF-8", &pair)
        }
        Cs::Jis0212 => {
            let triple = [0x8F, bytes[0] | 0x80, bytes[1] | 0x80];
            convert_oneshot("EUC-JP", "UTF-8", &triple)
        }
        Cs::Jis0213_2000 | Cs::Jis0213_2004 => {
            let pair = [bytes[0] | 0x80, bytes[1] | 0x80];
            convert_oneshot("EUC-JISX0213", "UTF-8", &pair)
        }
        Cs::Jis0213_2 => {
            let triple = [0x8F, bytes[0] | 0x80, bytes[1] | 0x80];
            convert_oneshot("EUC-JISX0213", "UTF-8", &triple)
        }
        Cs::Gb2312 | Cs::Ksx1001 => {
            let pair = [bytes[0] | 0x80, bytes[1] | 0x80];
            let encoding = if cs == Cs::Gb2312 { "GB2312" } else { "EUC-KR" };
            convert_oneshot(encoding, "UTF-8", &pair)
        }
        Cs::Ascii | Cs::None => None,
    }
}

/// Encodes one character in a designated ISO-2022 character set. DBCS
/// results are returned as EUC-style 8-bit bytes; single-byte G2 results are
/// already shifted into the 7-bit G2 plane.
fn encode_designation(cs: Cs, character: char) -> Option<Vec<u8>> {
    match cs {
        Cs::Roman => {
            let code = character as u32;
            match code {
                0x00..=0x7F if code != 0x5C && code != 0x7E => Some(vec![code as u8]),
                0x00A5 => Some(vec![0x5C]),
                0x203E => Some(vec![0x7E]),
                _ => None,
            }
        }
        Cs::Katakana => {
            let code = character as u32;
            if (0xFF61..=0xFF9F).contains(&code) {
                // 7-bit GL representation of the JIS X 0201 code.
                Some(vec![((code - 0xFEC0) - 0x80) as u8])
            } else {
                None
            }
        }
        Cs::Iso8859_1 => {
            let code = character as u32;
            if (0x80..=0xFF).contains(&code) {
                Some(vec![(code - 0x80) as u8])
            } else {
                None
            }
        }
        Cs::Iso8859_7 => {
            let bytes = convert_oneshot("UTF-8", "ISO-8859-7", character.to_string().as_bytes())?;
            if bytes.len() == 1 && bytes[0] >= 0x80 {
                Some(vec![bytes[0] - 0x80])
            } else {
                None
            }
        }
        Cs::Jis0208 => {
            let bytes = convert_oneshot("UTF-8", "EUC-JP", character.to_string().as_bytes())?;
            if bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b)) {
                Some(bytes)
            } else {
                None
            }
        }
        Cs::Jis0212 => {
            let bytes = convert_oneshot("UTF-8", "EUC-JP", character.to_string().as_bytes())?;
            if bytes.len() == 3
                && bytes[0] == 0x8F
                && (0xA1..=0xFE).contains(&bytes[1])
                && (0xA1..=0xFE).contains(&bytes[2])
            {
                Some(bytes[1..].to_vec())
            } else {
                None
            }
        }
        Cs::Jis0213_2000 | Cs::Jis0213_2004 => {
            let bytes = convert_oneshot("UTF-8", "EUC-JISX0213", character.to_string().as_bytes())?;
            if bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b)) {
                Some(bytes)
            } else {
                None
            }
        }
        Cs::Jis0213_2 => {
            let bytes = convert_oneshot("UTF-8", "EUC-JISX0213", character.to_string().as_bytes())?;
            if bytes.len() == 3
                && bytes[0] == 0x8F
                && (0xA1..=0xFE).contains(&bytes[1])
                && (0xA1..=0xFE).contains(&bytes[2])
            {
                Some(bytes[1..].to_vec())
            } else {
                None
            }
        }
        Cs::Gb2312 => {
            let bytes = convert_oneshot("UTF-8", "GB2312", character.to_string().as_bytes())?;
            if bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b)) {
                Some(bytes)
            } else {
                None
            }
        }
        Cs::Ksx1001 => {
            let bytes = convert_oneshot("UTF-8", "EUC-KR", character.to_string().as_bytes())?;
            if bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b)) {
                Some(bytes)
            } else {
                None
            }
        }
        Cs::Ascii | Cs::None => None,
    }
}

/// Tries the JIS X 0213 two-code-point mappings (for example `か` + the
/// combining handakuten). iconv combines these into one two-byte code.
fn encode_pair(config: &IsoConfig, pair: &str) -> Option<(Cs, Vec<u8>)> {
    for cs in config.order {
        if !matches!(cs, Cs::Jis0213_2000 | Cs::Jis0213_2004 | Cs::Jis0213_2) {
            continue;
        }
        let Some(bytes) = convert_oneshot("UTF-8", "EUC-JISX0213", pair.as_bytes()) else {
            continue;
        };
        if matches!(cs, Cs::Jis0213_2) {
            if bytes.len() == 3
                && bytes[0] == 0x8F
                && (0xA1..=0xFE).contains(&bytes[1])
                && (0xA1..=0xFE).contains(&bytes[2])
            {
                return Some((*cs, bytes[1..].to_vec()));
            }
        } else if bytes.len() == 2 && bytes.iter().all(|b| (0xA1..=0xFE).contains(b)) {
            return Some((*cs, bytes));
        }
    }
    None
}

fn emit_designation(out: &mut Vec<u8>, state: &mut IsoState, cs: Cs, plane: u8) {
    let mark = cs.mark();
    match plane {
        0 => {
            if state.shifted {
                out.push(0x0F);
                state.shifted = false;
            }
            if state.g[0] != cs {
                if cs.dbcs() {
                    if cs == Cs::Jis0208 {
                        out.extend_from_slice(&[0x1B, b'$', mark]);
                    } else {
                        out.extend_from_slice(&[0x1B, b'$', b'(', mark]);
                    }
                } else {
                    out.extend_from_slice(&[0x1B, b'(', mark]);
                }
                state.g[0] = cs;
            }
        }
        1 => {
            if state.g[1] != cs {
                if cs.dbcs() {
                    out.extend_from_slice(&[0x1B, b'$', b')', mark]);
                } else {
                    out.extend_from_slice(&[0x1B, b')', mark]);
                }
                state.g[1] = cs;
            }
            if !state.shifted {
                out.push(0x0E);
                state.shifted = true;
            }
        }
        _ => {
            if state.g[2] != cs {
                out.extend_from_slice(&[0x1B, b'.', mark]);
                state.g[2] = cs;
            }
            out.extend_from_slice(&[0x1B, b'N']);
        }
    }
}

/// Encodes `text` under an ISO-2022 configuration. `final_` appends the
/// terminal reset (ASCII designation / SI).
fn iso_encode(
    kind: Kind,
    config: &IsoConfig,
    state: &mut IsoState,
    text: &str,
    final_: bool,
    errors: &str,
) -> Result<Vec<u8>, i64> {
    let offsets = char_offsets(text);
    let chars = offsets.len() - 1;
    let mut out = Vec::with_capacity(text.len() + 16);
    let mut index = 0usize;
    while index < chars {
        let character = text[offsets[index]..].chars().next().unwrap_or('\u{0}');
        if (character as u32) < 0x80 {
            if state.g[0] != Cs::Ascii {
                out.extend_from_slice(&[0x1B, b'(', b'B']);
                state.g[0] = Cs::Ascii;
            }
            if state.shifted {
                out.push(0x0F);
                state.shifted = false;
            }
            out.push(character as u8);
            index += 1;
            continue;
        }
        if config.has_jis0213() && index + 1 < chars {
            let pair = &text[offsets[index]..offsets[index + 2]];
            if let Some((cs, bytes)) = encode_pair(config, pair) {
                let plane = config.plane_of(cs).unwrap_or(0);
                emit_designated(&mut out, state, cs, plane, &bytes);
                index += 2;
                continue;
            }
        }
        let mut chosen = None;
        for cs in config.order {
            if let Some(bytes) = encode_designation(*cs, character) {
                chosen = Some((*cs, bytes));
                break;
            }
        }
        match chosen {
            Some((cs, bytes)) => {
                let plane = config.plane_of(cs).unwrap_or(0);
                emit_designated(&mut out, state, cs, plane, &bytes);
                index += 1;
            }
            None => {
                let next = {
                    let mut encode_replacement = |replacement: &str,
                                                  target: &mut Vec<u8>|
                     -> Result<(), i64> {
                        let bytes =
                            iso_encode(kind, config, &mut *state, replacement, false, "strict")?;
                        target.extend_from_slice(&bytes);
                        Ok(())
                    };
                    handle_encode_error(
                        kind,
                        text,
                        &offsets,
                        index,
                        index + 1,
                        "illegal multibyte sequence",
                        errors,
                        &mut out,
                        &mut encode_replacement,
                    )?
                };
                index = next;
            }
        }
    }
    if final_ {
        if state.shifted {
            out.push(0x0F);
            state.shifted = false;
        }
        if state.g[0] != Cs::Ascii {
            out.extend_from_slice(&[0x1B, b'(', b'B']);
            state.g[0] = Cs::Ascii;
        }
    }
    Ok(out)
}

fn emit_designated(out: &mut Vec<u8>, state: &mut IsoState, cs: Cs, plane: u8, bytes: &[u8]) {
    emit_designation(out, state, cs, plane);
    if cs.dbcs() {
        out.push(bytes[0] & 0x7F);
        out.push(bytes[1] & 0x7F);
    } else if let Some(byte) = bytes.first() {
        out.push(*byte);
    }
}

// -------------------------------------------------------------------------
// Streaming entry points
// -------------------------------------------------------------------------

fn encode_text(kind: Kind, text: &str, errors: &str) -> Result<Vec<u8>, i64> {
    match kind {
        Kind::Hz => {
            let mut state = HzState::new();
            hz_encode(&mut state, text, true, errors)
        }
        kind if !kind.is_stateless() => {
            let config = iso_config(kind);
            let mut state = IsoState::encoder_start();
            iso_encode(kind, config, &mut state, text, true, errors)
        }
        kind => encode_stateless(kind, text, errors),
    }
}

fn decode_bytes(kind: Kind, data: &[u8], errors: &str) -> Result<Vec<u8>, i64> {
    let mut out = Vec::with_capacity(data.len());
    match kind {
        Kind::Hz => {
            let mut state = HzState::new();
            hz_decode(&mut state, data, true, errors, &mut out)?;
        }
        kind if !kind.is_stateless() => {
            let config = iso_config(kind);
            let mut state = IsoState::decoder_start();
            iso_decode(kind, config, &mut state, data, true, errors, &mut out)?;
        }
        kind => {
            decode_stateless(kind, data, true, errors, &mut out)?;
        }
    }
    Ok(out)
}

fn tuple_of(values: Vec<i64>) -> i64 {
    let roots = heap::root_values(&values);
    let result = value::tuple_new(values);
    heap::release_roots(roots);
    result
}

fn bytes_value(bytes: Vec<u8>) -> i64 {
    value::bytes_new(bytes)
}

fn text_value(text: String) -> i64 {
    value::str_alloc(text)
}

// -------------------------------------------------------------------------
// Codec objects and native classes
// -------------------------------------------------------------------------

/// Cached `_multibytecodec` class lookup. The registry keeps the class (and
/// therefore its methods) alive across calls and resets.
fn cached_class(key: &str, name: &str, methods: &[(&str, &'static str, usize)]) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let mut roots_vec = Vec::with_capacity(methods.len());
    let mut entries = Vec::with_capacity(methods.len());
    for (method_name, dispatch, arity) in methods {
        let function = value::native_function_new(MBC, dispatch, *arity);
        roots_vec.push(function);
        entries.push((method_name.to_string(), function, MethodKind::Normal));
    }
    let roots = heap::root_values(&roots_vec);
    let class = value::class_new_full(name.to_string(), NONE, entries, Vec::new());
    let class_roots = heap::root_values(std::slice::from_ref(&class));
    globals::registry_set(key, class);
    heap::release_roots(class_roots);
    heap::release_roots(roots);
    class
}

const CODEC_METHODS: &[(&str, &str, usize)] = &[
    ("__init__", "MultibyteCodec.__init__", 1),
    ("encode", "MultibyteCodec.encode", 3),
    ("decode", "MultibyteCodec.decode", 3),
];

const ENCODER_METHODS: &[(&str, &str, usize)] = &[
    ("__init__", "MultibyteIncrementalEncoder.__init__", 2),
    ("encode", "MultibyteIncrementalEncoder.encode", 3),
    ("getstate", "MultibyteIncrementalEncoder.getstate", 1),
    ("setstate", "MultibyteIncrementalEncoder.setstate", 2),
    ("reset", "MultibyteIncrementalEncoder.reset", 1),
];

const DECODER_METHODS: &[(&str, &str, usize)] = &[
    ("__init__", "MultibyteIncrementalDecoder.__init__", 2),
    ("decode", "MultibyteIncrementalDecoder.decode", 3),
    ("getstate", "MultibyteIncrementalDecoder.getstate", 1),
    ("setstate", "MultibyteIncrementalDecoder.setstate", 2),
    ("reset", "MultibyteIncrementalDecoder.reset", 1),
];

const STREAM_READER_METHODS: &[(&str, &str, usize)] =
    &[("__init__", "MultibyteStreamReader.__init__", 3)];

const STREAM_WRITER_METHODS: &[(&str, &str, usize)] =
    &[("__init__", "MultibyteStreamWriter.__init__", 3)];

fn multibytecodec_class() -> i64 {
    cached_class(
        "codecs_native.class.MultibyteCodec",
        "MultibyteCodec",
        CODEC_METHODS,
    )
}

fn encoder_class() -> i64 {
    cached_class(
        "codecs_native.class.MultibyteIncrementalEncoder",
        "MultibyteIncrementalEncoder",
        ENCODER_METHODS,
    )
}

fn decoder_class() -> i64 {
    cached_class(
        "codecs_native.class.MultibyteIncrementalDecoder",
        "MultibyteIncrementalDecoder",
        DECODER_METHODS,
    )
}

fn stream_reader_class() -> i64 {
    cached_class(
        "codecs_native.class.MultibyteStreamReader",
        "MultibyteStreamReader",
        STREAM_READER_METHODS,
    )
}

fn stream_writer_class() -> i64 {
    cached_class(
        "codecs_native.class.MultibyteStreamWriter",
        "MultibyteStreamWriter",
        STREAM_WRITER_METHODS,
    )
}

fn codec_instance(name: &str) -> i64 {
    let class = multibytecodec_class();
    let instance = value::instance_new(class);
    let name_value = value::str_alloc(name.to_string());
    let roots = heap::root_values(&[instance, name_value]);
    value::instance_setattr(instance, NAME_ATTR.to_string(), name_value);
    heap::release_roots(roots);
    instance
}

fn codec_name_of(value: i64) -> Option<&'static str> {
    let stored = value::instance_attr(value, NAME_ATTR)?;
    value::as_str(stored)
}

/// Resolves `self.codec` for an incremental codec instance, caching the name
/// on the instance the way CPython records the codec at construction.
fn resolve_instance_codec(instance: i64) -> Result<&'static str, i64> {
    if let Some(name) = codec_name_of(instance) {
        return Ok(name);
    }
    let Some(class) = value::instance_class_value(instance) else {
        return Err(type_error("codec is unexpected type"));
    };
    let Some(entry) = value::class_lookup(class, "codec") else {
        return Err(exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!(
                "'{}' object has no attribute 'codec'",
                value::type_name(instance)
            ),
        ));
    };
    match entry {
        value::ClassEntry::Attr(codec) => match codec_name_of(codec) {
            Some(name) => {
                let name_value = value::str_alloc(name.to_string());
                let roots = heap::root_values(&[instance, name_value]);
                value::instance_setattr(instance, NAME_ATTR.to_string(), name_value);
                heap::release_roots(roots);
                Ok(name)
            }
            None => Err(type_error("codec is unexpected type")),
        },
        value::ClassEntry::Method(..) => Err(type_error("codec is unexpected type")),
    }
}

fn stored_errors(instance: i64) -> &'static str {
    value::instance_attr(instance, ERRORS_ATTR)
        .and_then(value::as_str)
        .unwrap_or("strict")
}

fn stored_state_int(instance: i64) -> i64 {
    value::instance_attr(instance, STATE_ATTR)
        .and_then(value::as_integer)
        .unwrap_or(0)
}

fn store_state_int(instance: i64, value: i64) {
    value::instance_setattr(instance, STATE_ATTR.to_string(), ops::int_result(value));
}

fn stored_pending(instance: i64) -> Vec<u8> {
    value::instance_attr(instance, PENDING_ATTR)
        .and_then(value::as_bytes)
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default()
}

fn store_pending(instance: i64, bytes: Vec<u8>) {
    let value = value::bytes_new(bytes);
    let roots = heap::root_values(&[value]);
    value::instance_setattr(instance, PENDING_ATTR.to_string(), value);
    heap::release_roots(roots);
}

fn stored_pending_text(instance: i64) -> String {
    value::instance_attr(instance, PENDING_TEXT_ATTR)
        .and_then(value::as_str)
        .unwrap_or("")
        .to_string()
}

fn store_pending_text(instance: i64, text: String) {
    let value = value::str_alloc(text);
    let roots = heap::root_values(&[value]);
    value::instance_setattr(instance, PENDING_TEXT_ATTR.to_string(), value);
    heap::release_roots(roots);
}

fn errors_argument(value: Option<i64>, function: &str) -> Result<&'static str, i64> {
    match value {
        None | Some(NONE) => Ok("strict"),
        Some(argument) => match value::as_str(argument) {
            Some(name) => {
                // CPython validates the handler name at call/construction time.
                let name_value = value::str_alloc(name.to_string());
                let handler = crate::modules::codecs::call("lookup_error", &[name_value]);
                if exc::pending() {
                    return Err(NONE);
                }
                let _ = handler;
                Ok(name)
            }
            None => Err(type_error(&format!(
                "{function}() argument 'errors' must be str or None, not {}",
                value::type_name(argument)
            ))),
        },
    }
}

fn final_argument(value: Option<i64>) -> bool {
    value.map(value::truthy).unwrap_or(false)
}

fn text_of_argument(argument: i64) -> Result<&'static str, i64> {
    if let Some(text) = value::as_str(argument) {
        return Ok(text);
    }
    let converted = core_builtins::to_str(argument);
    if exc::pending() {
        return Err(NONE);
    }
    match value::as_str(converted) {
        Some(text) => Ok(text),
        None => Err(type_error("couldn't convert the object to unicode.")),
    }
}

fn buffer_of_argument(argument: i64) -> Option<Vec<u8>> {
    if let Some(bytes) = value::as_bytes(argument) {
        return Some(bytes.to_vec());
    }
    if let Some(bytes) = value::as_bytearray(argument) {
        return Some(bytes.clone());
    }
    if let Some(view) = value::as_memoryview(argument) {
        return value::memoryview_bytes(view).ok();
    }
    None
}

/// Reads an ISO-2022 instance's packed state, falling back to the start state.
fn iso_state_of(instance: i64, encoder: bool) -> IsoState {
    let start = if encoder {
        IsoState::encoder_start()
    } else {
        IsoState::decoder_start()
    };
    match value::instance_attr(instance, STATE_ATTR).and_then(value::as_integer) {
        Some(state) => IsoState::from_value(state).unwrap_or(start),
        None => start,
    }
}

fn is_pair_start(character: char) -> bool {
    matches!(
        character,
        '\u{00E6}'
            | '\u{0254}'
            | '\u{0259}'
            | '\u{025A}'
            | '\u{028C}'
            | '\u{02E5}'
            | '\u{02E9}'
            | '\u{304B}'
            | '\u{304D}'
            | '\u{304F}'
            | '\u{3051}'
            | '\u{3053}'
            | '\u{30AB}'
            | '\u{30AD}'
            | '\u{30AF}'
            | '\u{30B1}'
            | '\u{30B3}'
            | '\u{30BB}'
            | '\u{30C4}'
            | '\u{30C8}'
            | '\u{31F7}'
    )
}

// -------------------------------------------------------------------------
// Method implementations
// -------------------------------------------------------------------------

fn codec_init(args: &[i64]) -> i64 {
    if args.is_empty() {
        return type_error("MultibyteCodec.__init__() missing self");
    }
    type_error("cannot create '_multibytecodec.MultibyteCodec' instances")
}

fn codec_encode_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("encode() missing self");
    };
    let Some(&input) = args.get(1) else {
        return type_error("encode() missing input");
    };
    let errors = match errors_argument(args.get(2).copied(), "encode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let text = match text_of_argument(input) {
        Ok(text) => text,
        Err(error) => return error,
    };
    match encode_text(kind, text, errors) {
        Ok(bytes) => {
            let characters = text.chars().count() as i64;
            let value = bytes_value(bytes);
            tuple_of(vec![value, ops::int_result(characters)])
        }
        Err(error) => error,
    }
}

fn codec_decode_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("decode() missing self");
    };
    let Some(&input) = args.get(1) else {
        return type_error("decode() missing input");
    };
    let errors = match errors_argument(args.get(2).copied(), "decode") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let Some(data) = buffer_of_argument(input) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(input)
        ));
    };
    match decode_bytes(kind, &data, errors) {
        Ok(text) => {
            let text = match String::from_utf8(text) {
                Ok(text) => text,
                Err(_) => return runtime_error("decoded text is not valid UTF-8"),
            };
            let length = ops::int_result(data.len() as i64);
            tuple_of(vec![text_value(text), length])
        }
        Err(error) => error,
    }
}

fn incremental_encoder_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("MultibyteIncrementalEncoder.__init__() missing self");
    };
    let errors = match errors_argument(args.get(1).copied(), "IncrementalEncoder") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    store_string(instance, ERRORS_ATTR, errors);
    store_pending(instance, Vec::new());
    store_pending_text(instance, String::new());
    let state = kind_named(name)
        .filter(|kind| kind.is_iso2022())
        .map(|_| IsoState::encoder_start().to_value())
        .unwrap_or(0);
    store_state_int(instance, state);
    NONE
}

fn incremental_decoder_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("MultibyteIncrementalDecoder.__init__() missing self");
    };
    let errors = match errors_argument(args.get(1).copied(), "IncrementalDecoder") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    store_string(instance, ERRORS_ATTR, errors);
    store_pending(instance, Vec::new());
    let state = kind_named(name)
        .filter(|kind| kind.is_iso2022())
        .map(|_| IsoState::decoder_start().to_value())
        .unwrap_or(0);
    store_state_int(instance, state);
    NONE
}

fn stream_init(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("MultibyteStreamReader.__init__() missing self");
    };
    let errors = match errors_argument(args.get(2).copied(), "MultibyteStream") {
        Ok(errors) => errors,
        Err(error) => return error,
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let _ = kind_named(name);
    if let Some(&stream) = args.get(1) {
        let roots = heap::root_values(&[stream]);
        value::instance_setattr(instance, "stream".to_string(), stream);
        heap::release_roots(roots);
    }
    store_string(instance, ERRORS_ATTR, errors);
    NONE
}

fn store_string(instance: i64, attribute: &str, text: &str) {
    let value = value::str_alloc(text.to_string());
    let roots = heap::root_values(&[value]);
    value::instance_setattr(instance, attribute.to_string(), value);
    heap::release_roots(roots);
}

fn encoder_encode_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("encode() missing self");
    };
    let Some(&input) = args.get(1) else {
        return type_error("encode() missing input");
    };
    let final_ = final_argument(args.get(2).copied());
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let errors = stored_errors(instance);
    let text = match text_of_argument(input) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let pending = stored_pending_text(instance);
    let mut combined = pending;
    combined.push_str(text);
    let mut keep = String::new();
    if kind.pairs_across_calls() && !final_ {
        if let Some(last) = combined.chars().last() {
            if is_pair_start(last) {
                let start = combined.len() - last.len_utf8();
                keep = combined.split_off(start);
            }
        }
    }
    let prefix = combined;
    let result = if kind.is_stateless() {
        encode_stateless(kind, &prefix, errors)
    } else if kind == Kind::Hz {
        let mut state = HzState {
            gb: stored_state_int(instance) != 0,
        };
        let result = hz_encode(&mut state, &prefix, final_, errors);
        store_state_int(instance, i64::from(state.gb));
        result
    } else {
        let config = iso_config(kind);
        let mut state = iso_state_of(instance, true);
        let result = iso_encode(
            kind,
            config,
            &mut state,
            &prefix,
            final_ && keep.is_empty(),
            errors,
        );
        store_state_int(instance, state.to_value());
        result
    };
    match result {
        Ok(bytes) => {
            store_pending_text(instance, keep);
            bytes_value(bytes)
        }
        Err(error) => error,
    }
}

fn decoder_decode_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("decode() missing self");
    };
    let Some(&input) = args.get(1) else {
        return type_error("decode() missing input");
    };
    let final_ = final_argument(args.get(2).copied());
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let errors = stored_errors(instance);
    let Some(data) = buffer_of_argument(input) else {
        return type_error(&format!(
            "a bytes-like object is required, not '{}'",
            value::type_name(input)
        ));
    };
    let pending = stored_pending(instance);
    let mut combined = pending;
    combined.extend_from_slice(&data);
    let mut out = Vec::with_capacity(combined.len());
    let outcome = if kind.is_stateless() {
        decode_stateless(kind, &combined, final_, errors, &mut out).map(|consumed| {
            let pending = combined[consumed..].to_vec();
            store_pending(instance, pending);
        })
    } else if kind == Kind::Hz {
        let previous = stored_state_int(instance) != 0;
        let mut state = HzState { gb: previous };
        let result = hz_decode(&mut state, &combined, final_, errors, &mut out);
        match result {
            Ok(consumed) => {
                store_state_int(instance, i64::from(state.gb));
                store_pending(instance, combined[consumed..].to_vec());
                Ok(())
            }
            Err(error) => {
                store_state_int(instance, i64::from(previous));
                Err(error)
            }
        }
    } else {
        let previous = iso_state_of(instance, false);
        let config = iso_config(kind);
        let mut state = previous;
        let result = iso_decode(
            kind, config, &mut state, &combined, final_, errors, &mut out,
        );
        match result {
            Ok(consumed) => {
                store_state_int(instance, state.to_value());
                store_pending(instance, combined[consumed..].to_vec());
                Ok(())
            }
            Err(error) => {
                store_state_int(instance, previous.to_value());
                Err(error)
            }
        }
    };
    if let Err(error) = outcome {
        return error;
    }
    match String::from_utf8(out) {
        Ok(text) => text_value(text),
        Err(_) => runtime_error("decoded text is not valid UTF-8"),
    }
}

fn state_bytes(instance: i64, kind: Kind, encoder: bool) -> [u8; 8] {
    if kind.is_stateless() {
        [0u8; 8]
    } else if kind == Kind::Hz {
        let mut bytes = [0u8; 8];
        bytes[0] = u8::from(stored_state_int(instance) != 0);
        bytes
    } else {
        iso_state_of(instance, encoder).raw_bytes()
    }
}

fn state_int(bytes: [u8; 8]) -> i64 {
    use num_bigint::{BigInt, Sign};
    let number = BigInt::from_bytes_le(Sign::Plus, &bytes);
    value::bigint_new(number)
}

fn encoder_getstate_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("getstate() missing self");
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let pending = stored_pending_text(instance);
    let pending_bytes = pending.as_bytes();
    if pending_bytes.len() > 8 {
        return exc::raise_with_message(UNICODE_ENCODE_ERROR, "pending buffer too large");
    }
    let mut packed = Vec::with_capacity(1 + pending_bytes.len() + 8);
    packed.push(pending_bytes.len() as u8);
    packed.extend_from_slice(pending_bytes);
    packed.extend_from_slice(&state_bytes(instance, kind, true));
    use num_bigint::{BigInt, Sign};
    value::bigint_new(BigInt::from_bytes_le(Sign::Plus, &packed))
}

fn decoder_getstate_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("getstate() missing self");
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(kind) = kind_named(name) else {
        return lookup_error("no such codec is supported.");
    };
    let pending = bytes_value(stored_pending(instance));
    let state = state_int(state_bytes(instance, kind, false));
    tuple_of(vec![pending, state])
}

/// Splits a little-endian integer into the pending buffer and 8 state bytes,
/// mirroring CPython's fixed 17-byte encoder buffer.
fn unpack_encoder_state(instance: i64, value: i64) -> Result<(), i64> {
    use num_bigint::Sign;
    let Some(number) = value::to_bigint(value) else {
        return Err(type_error("setstate() state must be an int"));
    };
    let (sign, bytes) = number.to_bytes_le();
    if sign == Sign::Minus {
        return Err(exc::raise_with_message(
            UNICODE_ERROR,
            "pending buffer too large",
        ));
    }
    let length = bytes.first().copied().unwrap_or(0) as usize;
    if length > 8 {
        return Err(exc::raise_with_message(
            UNICODE_ERROR,
            "pending buffer too large",
        ));
    }
    let mut buffer = vec![0u8; length];
    for (index, slot) in buffer.iter_mut().enumerate() {
        *slot = bytes.get(1 + index).copied().unwrap_or(0);
    }
    let pending = match std::str::from_utf8(&buffer) {
        Ok(text) => text.to_string(),
        Err(_) => {
            return Err(exc::raise_with_message(
                UNICODE_DECODE_ERROR,
                "invalid UTF-8 in codec state",
            ))
        }
    };
    let mut state = [0u8; 8];
    for (index, slot) in state.iter_mut().enumerate() {
        *slot = bytes.get(1 + length + index).copied().unwrap_or(0);
    }
    store_pending_text(instance, pending);
    store_state_int(instance, i64::from_le_bytes(state));
    Ok(())
}

fn encoder_setstate_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("setstate() missing self");
    };
    let Some(&state) = args.get(1) else {
        return type_error("setstate() missing state");
    };
    match unpack_encoder_state(instance, state) {
        Ok(()) => NONE,
        Err(error) => error,
    }
}

fn decoder_setstate_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("setstate() missing self");
    };
    let Some(&state) = args.get(1) else {
        return type_error("setstate() missing state");
    };
    let Some(items) = value::as_tuple(state) else {
        return type_error("setstate(): illegal state argument");
    };
    if items.len() != 2 {
        return type_error("setstate(): illegal state argument");
    }
    let Some(pending) = value::as_bytes(items[0]) else {
        return type_error("setstate(): illegal state argument");
    };
    if pending.len() > 8 {
        return exc::raise_with_message(UNICODE_DECODE_ERROR, "pending buffer too large");
    }
    let Some(number) = value::to_bigint(items[1]) else {
        return type_error("setstate(): illegal state argument");
    };
    let (sign, bytes) = number.to_bytes_le();
    if sign == num_bigint::Sign::Minus {
        return exc::raise_with_message(UNICODE_ERROR, "pending buffer too large");
    }
    let mut packed = [0u8; 8];
    for (index, slot) in packed.iter_mut().enumerate() {
        *slot = bytes.get(index).copied().unwrap_or(0);
    }
    store_pending(instance, pending.to_vec());
    store_state_int(instance, i64::from_le_bytes(packed));
    NONE
}

fn encoder_reset_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("reset() missing self");
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let state = match kind_named(name) {
        Some(kind) if kind.is_iso2022() => {
            // CPython's reset only returns G0 to ASCII and clears the shift;
            // the G1/G2 designations stay as they were.
            let mut state = iso_state_of(instance, true);
            state.g[0] = Cs::Ascii;
            state.shifted = false;
            state.to_value()
        }
        _ => 0,
    };
    store_state_int(instance, state);
    store_pending_text(instance, String::new());
    store_pending(instance, Vec::new());
    NONE
}

fn decoder_reset_method(args: &[i64]) -> i64 {
    let Some(&instance) = args.first() else {
        return type_error("reset() missing self");
    };
    let name = match resolve_instance_codec(instance) {
        Ok(name) => name,
        Err(error) => return error,
    };
    let state = match kind_named(name) {
        Some(kind) if kind.is_iso2022() => {
            let mut state = iso_state_of(instance, false);
            state.g[0] = Cs::Ascii;
            state.shifted = false;
            state.to_value()
        }
        _ => 0,
    };
    store_state_int(instance, state);
    store_pending(instance, Vec::new());
    NONE
}

// -------------------------------------------------------------------------
// Public module surface
// -------------------------------------------------------------------------

fn attribute_error(module: &str, name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{module}' has no attribute '{name}'"),
    )
}

/// `_codecs_xx.getcodec(name)`.
fn getcodec(module: &str, args: &[i64]) -> i64 {
    let Some(&argument) = args.first() else {
        return type_error("getcodec() missing required argument 'encoding'");
    };
    let Some(name) = value::as_str(argument) else {
        return type_error("encoding name must be a string.");
    };
    let Some(entries) = module_codecs(module) else {
        return lookup_error("no such codec is supported.");
    };
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        return lookup_error("no such codec is supported.");
    };
    if !kind_supported(entry.kind) {
        return lookup_error(&format!(
            "codec '{}' is not available on this system",
            entry.name
        ));
    }
    codec_instance(entry.name)
}

/// `_multibytecodec.__create_codec(codec)`: CPython wraps a C capsule here;
/// this implementation only ever produces its own codec objects, so the
/// argument is the object to return.
fn create_codec(args: &[i64]) -> i64 {
    let Some(&argument) = args.first() else {
        return type_error("__create_codec() missing required argument");
    };
    if codec_name_of(argument).is_some() {
        return argument;
    }
    type_error("codec is unexpected type")
}

/// Keyword parameters of a dispatcher, including `self`.
fn parameter_names(function: &str) -> &'static [&'static str] {
    match function {
        "MultibyteCodec.encode" | "MultibyteCodec.decode" => &["self", "input", "errors"],
        "MultibyteIncrementalEncoder.encode" | "MultibyteIncrementalDecoder.decode" => {
            &["self", "input", "final"]
        }
        "MultibyteIncrementalEncoder.__init__" | "MultibyteIncrementalDecoder.__init__" => {
            &["self", "errors"]
        }
        "MultibyteStreamReader.__init__" | "MultibyteStreamWriter.__init__" => {
            &["self", "stream", "errors"]
        }
        "MultibyteIncrementalEncoder.setstate" | "MultibyteIncrementalDecoder.setstate" => {
            &["self", "state"]
        }
        "MultibyteIncrementalEncoder.getstate"
        | "MultibyteIncrementalDecoder.getstate"
        | "MultibyteIncrementalEncoder.reset"
        | "MultibyteIncrementalDecoder.reset"
        | "MultibyteCodec.__init__" => &["self"],
        _ => &[],
    }
}

/// Keyword-aware dispatch. The runtime keyword binder usually fills the
/// positional slots before this runs; the manual binding below keeps direct
/// ABI calls working as well.
pub(crate) fn call_kw(
    module: &str,
    function: &str,
    args: &[i64],
    keywords: &[(String, i64)],
) -> i64 {
    if keywords.is_empty() {
        return call(module, function, args);
    }
    let parameters = parameter_names(function);
    if parameters.is_empty() {
        return call(module, function, args);
    }
    let mut argv = args.to_vec();
    for (name, value) in keywords {
        let Some(index) = parameters
            .iter()
            .position(|parameter| *parameter == name.as_str())
        else {
            return type_error(&format!(
                "{module}.{function}() got an unexpected keyword argument '{name}'"
            ));
        };
        if index < argv.len() {
            return type_error(&format!(
                "{module}.{function}() got multiple values for argument '{name}'"
            ));
        }
        while argv.len() < index {
            argv.push(NONE);
        }
        argv.push(*value);
    }
    call(module, function, &argv)
}

/// Module constants. Only `_multibytecodec` has any.
pub(crate) fn constant(module: &str, name: &str) -> Option<i64> {
    match (module, name) {
        ("_multibytecodec", "MultibyteCodec") => Some(multibytecodec_class()),
        ("_multibytecodec", "MultibyteIncrementalEncoder") => Some(encoder_class()),
        ("_multibytecodec", "MultibyteIncrementalDecoder") => Some(decoder_class()),
        ("_multibytecodec", "MultibyteStreamReader") => Some(stream_reader_class()),
        ("_multibytecodec", "MultibyteStreamWriter") => Some(stream_writer_class()),
        _ => None,
    }
}

/// Dispatches a call on one of the seven codec accelerator modules.
pub(crate) fn call(module: &str, function: &str, args: &[i64]) -> i64 {
    match module {
        "_codecs_cn" | "_codecs_jp" | "_codecs_kr" | "_codecs_tw" | "_codecs_hk"
        | "_codecs_iso2022" => match function {
            "getcodec" => getcodec(module, args),
            _ => attribute_error(module, function),
        },
        MBC => match function {
            "__create_codec" => create_codec(args),
            "MultibyteCodec.__init__" => codec_init(args),
            "MultibyteCodec.encode" => codec_encode_method(args),
            "MultibyteCodec.decode" => codec_decode_method(args),
            "MultibyteIncrementalEncoder.__init__" => incremental_encoder_init(args),
            "MultibyteIncrementalEncoder.encode" => encoder_encode_method(args),
            "MultibyteIncrementalEncoder.getstate" => encoder_getstate_method(args),
            "MultibyteIncrementalEncoder.setstate" => encoder_setstate_method(args),
            "MultibyteIncrementalEncoder.reset" => encoder_reset_method(args),
            "MultibyteIncrementalDecoder.__init__" => incremental_decoder_init(args),
            "MultibyteIncrementalDecoder.decode" => decoder_decode_method(args),
            "MultibyteIncrementalDecoder.getstate" => decoder_getstate_method(args),
            "MultibyteIncrementalDecoder.setstate" => decoder_setstate_method(args),
            "MultibyteIncrementalDecoder.reset" => decoder_reset_method(args),
            "MultibyteStreamReader.__init__" | "MultibyteStreamWriter.__init__" => {
                stream_init(args)
            }
            _ => attribute_error(module, function),
        },
        _ => attribute_error(module, function),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> i64 {
        value::str_alloc(value.to_string())
    }

    fn bytes(value: &[u8]) -> i64 {
        value::bytes_new(value.to_vec())
    }

    fn codec(module: &str, name: &str) -> i64 {
        call(module, "getcodec", &[text(name)])
    }

    fn encode(codec: i64, input: &str, errors: &str) -> i64 {
        call(
            MBC,
            "MultibyteCodec.encode",
            &[codec, text(input), text(errors)],
        )
    }

    fn decode(codec: i64, input: &[u8], errors: &str) -> i64 {
        call(
            MBC,
            "MultibyteCodec.decode",
            &[codec, bytes(input), text(errors)],
        )
    }

    fn tuple_of(value: i64) -> Vec<i64> {
        value::as_tuple(value).expect("tuple").to_vec()
    }

    fn encoded(value: i64) -> Vec<u8> {
        let items = tuple_of(value);
        value::as_bytes(items[0]).expect("bytes").to_vec()
    }

    fn decoded(value: i64) -> String {
        let items = tuple_of(value);
        value::as_str(items[0]).expect("str").to_string()
    }

    fn error_tag() -> u32 {
        exc::pending_tag() as u32
    }

    fn clear_error() {
        exc::clear();
    }

    fn incremental(class: i64, name: &str) -> i64 {
        let instance = value::instance_new(class);
        let hidden = value::str_alloc(name.to_string());
        let strict = value::str_alloc("strict".to_string());
        let roots = heap::root_values(&[instance, hidden, strict]);
        value::instance_setattr(instance, NAME_ATTR.to_string(), hidden);
        value::instance_setattr(instance, ERRORS_ATTR.to_string(), strict);
        heap::release_roots(roots);
        instance
    }

    #[test]
    fn common_codecs_round_trip_known_vectors() {
        exc::init();
        for (module, name, encoded_bytes) in [
            ("_codecs_cn", "gb2312", &b"\xd6\xd0\xce\xc4"[..]),
            ("_codecs_cn", "gbk", &b"\xd6\xd0\xce\xc4"[..]),
            ("_codecs_cn", "gb18030", &b"\xd6\xd0\xce\xc4"[..]),
            ("_codecs_tw", "big5", &b"\xa4\xa4\xa4\xe5"[..]),
            ("_codecs_tw", "cp950", &b"\xa4\xa4\xa4\xe5"[..]),
            ("_codecs_hk", "big5hkscs", &b"\xa4\xa4\xa4\xe5"[..]),
            ("_codecs_jp", "shift_jis", &b"\x93\xfa\x96\x7b\x8c\xea"[..]),
            ("_codecs_jp", "euc_jp", &b"\xc6\xfc\xcb\xdc\xb8\xec"[..]),
        ] {
            let handle = codec(module, name);
            assert!(!exc::pending(), "{name}: getcodec failed");
            let input = if name == "shift_jis" || name == "euc_jp" {
                "日本語"
            } else {
                "中文"
            };
            assert_eq!(
                encoded(encode(handle, input, "strict")),
                encoded_bytes,
                "{name} encode"
            );
            assert_eq!(decoded(decode(handle, encoded_bytes, "strict")), input);
        }
        let korean = codec("_codecs_kr", "cp949");
        assert_eq!(
            encoded(encode(korean, "한국", "strict")),
            &b"\xc7\xd1\xb1\xb9"[..]
        );
        assert_eq!(
            decoded(decode(korean, b"\xc7\xd1\xb1\xb9", "strict")),
            "한국"
        );
    }

    #[test]
    fn gb18030_encodes_supplementary_code_points() {
        exc::init();
        let handle = codec("_codecs_cn", "gb18030");
        assert_eq!(
            encoded(encode(handle, "😀", "strict")),
            &b"\x94\x39\xfc\x36"[..]
        );
        assert_eq!(decoded(decode(handle, b"\x94\x39\xfc\x36", "strict")), "😀");
    }

    #[test]
    fn invalid_bytes_follow_the_error_handler() {
        exc::init();
        let handle = codec("_codecs_cn", "gbk");
        assert_eq!(
            decoded(decode(handle, b"abc\x80\x80\xc1\xc4", "replace")),
            "abc\u{FFFD}\u{FFFD}\u{804A}"
        );
        assert_eq!(
            decoded(decode(handle, b"abc\x80\x80\xc1\xc4", "ignore")),
            "abc\u{804A}"
        );
        assert_eq!(
            decoded(decode(handle, b"abc\x84\x39\x84\x39\xc1\xc4", "replace")),
            "abc\u{FFFD}9\u{FFFD}9\u{804A}"
        );
        let failure = decode(handle, b"abc\x80", "strict");
        assert_eq!(error_tag(), UNICODE_DECODE_ERROR);
        clear_error();
        assert_eq!(failure, NONE);
    }

    #[test]
    fn encoding_errors_use_the_registered_handlers() {
        exc::init();
        let handle = codec("_codecs_cn", "gbk");
        assert_eq!(encoded(encode(handle, "\u{30FB}", "replace")), &b"?"[..]);
        assert!(encode(handle, "\u{30FB}", "strict") == NONE);
        assert_eq!(error_tag(), UNICODE_ENCODE_ERROR);
        clear_error();
    }

    #[test]
    fn iso2022_jp_matches_cpython_vectors() {
        exc::init();
        let handle = codec("_codecs_iso2022", "iso2022_jp");
        assert_eq!(
            encoded(encode(handle, "日本語", "strict")),
            &b"\x1b$BF|K\\8l\x1b(B"[..]
        );
        assert_eq!(
            decoded(decode(handle, b"\x1b$BF|K\\8l\x1b(B", "strict")),
            "日本語"
        );
        assert_eq!(
            decoded(decode(handle, b"ab\x1Bdef", "replace")),
            "ab\x1bdef"
        );
        assert_eq!(
            decoded(decode(handle, b"ab\x1B$def", "replace")),
            "ab\u{FFFD}"
        );
    }

    #[test]
    fn iso2022_kr_uses_the_designation_only_when_needed() {
        exc::init();
        let handle = codec("_codecs_iso2022", "iso2022_kr");
        assert_eq!(encoded(encode(handle, "abc", "strict")), &b"abc"[..]);
        assert_eq!(
            encoded(encode(handle, "한국", "strict")),
            &b"\x1b$)C\x0eGQ19\x0f"[..]
        );
        assert_eq!(
            decoded(decode(handle, b"\x1b$)C\x0eGQ19\x0f", "strict")),
            "한국"
        );
    }

    #[test]
    fn hz_switches_between_ascii_and_gb2312() {
        exc::init();
        let handle = codec("_codecs_cn", "hz");
        assert_eq!(encoded(encode(handle, "中文", "strict")), &b"~{VPND~}"[..]);
        assert_eq!(
            encoded(encode(handle, "~中文\n", "strict")),
            &b"~~~{VPND~}\n"[..]
        );
        assert_eq!(decoded(decode(handle, b"~{VPND~}", "strict")), "中文");
        assert_eq!(decoded(decode(handle, b"~~", "strict")), "~");
        assert_eq!(decoded(decode(handle, b"~\n~{VPND~}", "strict")), "中文");
        assert!(decode(handle, b"~}", "strict") == NONE);
        assert_eq!(error_tag(), UNICODE_DECODE_ERROR);
        clear_error();
    }

    #[test]
    fn iso2022_incremental_state_matches_cpython_layout() {
        exc::init();
        let encoder = incremental(encoder_class(), "iso2022_jp");
        let initial = call(MBC, "MultibyteIncrementalEncoder.getstate", &[encoder]);
        assert_eq!(value::as_integer(initial), Some(0x42_42_00));
        let first = call(
            MBC,
            "MultibyteIncrementalEncoder.encode",
            &[encoder, text("あ")],
        );
        assert_eq!(value::as_bytes(first).unwrap(), &b"\x1b$B$\""[..]);
        let after = call(MBC, "MultibyteIncrementalEncoder.getstate", &[encoder]);
        assert_eq!(value::as_integer(after), Some(0x42_C2_00));
        let second = call(
            MBC,
            "MultibyteIncrementalEncoder.encode",
            &[encoder, text("z")],
        );
        assert_eq!(value::as_bytes(second).unwrap(), &b"\x1b(Bz"[..]);

        let decoder = incremental(decoder_class(), "iso2022_jp");
        let initial_decoder = call(MBC, "MultibyteIncrementalDecoder.getstate", &[decoder]);
        let items = tuple_of(initial_decoder);
        assert!(value::as_bytes(items[0]).unwrap().is_empty());
        assert_eq!(value::as_integer(items[1]), Some(0x42_42_42));

        // A complete designation is consumed: CPython leaves no pending bytes
        // and keeps the designated charset in the state.
        let first = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[decoder, bytes(b"\x1b$B")],
        );
        assert_eq!(value::as_str(first).unwrap(), "");
        assert!(stored_pending(decoder).is_empty());
        let second = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[decoder, bytes(b"\x24\x22")],
        );
        assert_eq!(value::as_str(second).unwrap(), "あ");
        assert!(stored_pending(decoder).is_empty());

        // A partial designation stays pending until the next call completes it.
        let buffered = incremental(decoder_class(), "iso2022_jp");
        let first = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[buffered, bytes(b"\x1b$")],
        );
        assert_eq!(value::as_str(first).unwrap(), "");
        assert_eq!(stored_pending(buffered), b"\x1b$".to_vec());
        let second = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[buffered, bytes(b"B\x24\x22")],
        );
        assert_eq!(value::as_str(second).unwrap(), "あ");
        assert!(stored_pending(buffered).is_empty());
    }

    #[test]
    fn incremental_decoder_buffers_partial_sequences() {
        exc::init();
        let decoder = incremental(decoder_class(), "gbk");
        let first = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[decoder, bytes(b"\xd6")],
        );
        assert_eq!(value::as_str(first).unwrap(), "");
        assert_eq!(stored_pending(decoder), b"\xd6".to_vec());
        let second = call(
            MBC,
            "MultibyteIncrementalDecoder.decode",
            &[decoder, bytes(b"\xd0")],
        );
        assert_eq!(value::as_str(second).unwrap(), "中");
    }

    #[test]
    fn classes_and_constants_are_cached() {
        exc::init();
        let class = constant(MBC, "MultibyteCodec").expect("codec class");
        assert_eq!(class, multibytecodec_class());
        assert_eq!(
            constant(MBC, "MultibyteIncrementalEncoder"),
            Some(encoder_class())
        );
        assert_eq!(
            constant(MBC, "MultibyteStreamReader"),
            Some(stream_reader_class())
        );
        assert_eq!(
            constant(MBC, "MultibyteStreamWriter"),
            Some(stream_writer_class())
        );
        assert!(value::class_method(class, "encode").is_some());
        assert!(value::class_method(class, "decode").is_some());
    }

    #[test]
    fn getcodec_rejects_unknown_and_foreign_names() {
        exc::init();
        assert!(codec("_codecs_cn", "big5") == NONE);
        assert_eq!(error_tag(), LOOKUP_ERROR);
        clear_error();
        assert!(codec("_codecs_jp", "gbk") == NONE);
        assert_eq!(error_tag(), LOOKUP_ERROR);
        clear_error();
        assert!(getcodec("_codecs_cn", &[ops::int_result(1)]) == NONE);
        assert_eq!(error_tag(), TYPE_ERROR);
        clear_error();
    }
}
