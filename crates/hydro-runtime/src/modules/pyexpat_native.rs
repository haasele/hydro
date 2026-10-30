//! `pyexpat`: CPython's interface to the Expat non-validating XML parser,
//! implemented on the system `libexpat` through a direct FFI binding.
//!
//! The module owns one Expat parser per `xmlparser` instance. The Expat
//! `userData` pointer carries a small integer handle into a process-wide
//! registry; the registry holds the raw `XML_Parser`, the Python instance and
//! the character-data buffer. Callbacks convert Expat's UTF-8 strings into
//! Hydro strings, intern element/attribute names through the parser's
//! `intern` dictionary (when enabled), and call the Python handler stored on
//! the instance. A Python handler that raises stops the Expat parser, and the
//! pending exception is returned unchanged to the caller of `Parse`/
//! `ParseFile`.
//!
//! Supported surface:
//!
//! * `ParserCreate(encoding=None, namespace_separator=None, intern=...)`,
//!   `ErrorString(code)`, `XMLParserType`, `ExpatError`/`error`,
//!   `EXPAT_VERSION`, `version_info`, `native_encoding`, `features`, the
//!   `XML_PARAM_ENTITY_PARSING_*` constants, the `errors` and `model` objects.
//! * `Parse`, `ParseFile`, `SetBase`, `GetBase`,
//!   `ExternalEntityParserCreate`, `SetParamEntityParsing` and `UseForeignDTD`.
//! * Every CPython handler attribute, including the eight properties that
//!   expose the parse position and the parser tuning flags (`buffer_text`,
//!   `buffer_size`, `buffer_used`, `ordered_attributes`,
//!   `specified_attributes`, `namespace_prefixes`).
//!
//! `XML_Char` is assumed to be 8-bit, which is how system libexpat is built
//! on the platforms Hydro supports (the same assumption CPython's vendored
//! copy makes). The parser registry intentionally lives for the process: the
//! runtime has no finaliser hook for native instances, so a discarded parser
//! keeps its Expat allocation until exit, exactly like the streaming objects
//! of the native `zlib` module.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::builtins;
use crate::container;
use crate::core::globals;
use crate::exc::{
    self, ATTRIBUTE_ERROR, EXCEPTION, MEMORY_ERROR, RUNTIME_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use crate::heap;
use crate::methods;
use crate::value::{self, NONE};

// -------------------------------------------------------------------------
// libexpat FFI
// -------------------------------------------------------------------------

type XmlParser = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct XmlContent {
    kind: c_int,
    quant: c_int,
    name: *mut c_char,
    num_children: c_uint,
    children: *mut XmlContent,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XmlExpatVersion {
    major: c_int,
    minor: c_int,
    micro: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XmlFeature {
    feature: c_int,
    name: *const c_char,
    value: c_long,
}

type StartElementHandler = unsafe extern "C" fn(*mut c_void, *const c_char, *const *const c_char);
type EndElementHandler = unsafe extern "C" fn(*mut c_void, *const c_char);
type CharacterDataHandler = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);
type ProcessingInstructionHandler = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);
type CommentHandler = unsafe extern "C" fn(*mut c_void, *const c_char);
type CdataSectionHandler = unsafe extern "C" fn(*mut c_void);
type DefaultHandler = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);
type EndDoctypeHandler = unsafe extern "C" fn(*mut c_void);
type StartDoctypeHandler =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, c_int);
type EntityDeclHandler = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    c_int,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
);
type NotationDeclHandler =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *const c_char, *const c_char);
type StartNamespaceDeclHandler = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char);
type EndNamespaceDeclHandler = unsafe extern "C" fn(*mut c_void, *const c_char);
type NotStandaloneHandler = unsafe extern "C" fn(*mut c_void) -> c_int;
type ExternalEntityRefHandler = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
) -> c_int;
type XmlDeclHandler = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int);
type ElementDeclHandler = unsafe extern "C" fn(*mut c_void, *const c_char, *mut XmlContent);
type AttlistDeclHandler = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    c_int,
);
type SkippedEntityHandler = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);
type UnparsedEntityDeclHandler = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
);

#[link(name = "expat")]
extern "C" {
    fn XML_ParserCreate(encoding: *const c_char) -> XmlParser;
    fn XML_ParserCreateNS(encoding: *const c_char, separator: c_char) -> XmlParser;
    fn XML_ExternalEntityParserCreate(
        parser: XmlParser,
        context: *const c_char,
        encoding: *const c_char,
    ) -> XmlParser;
    fn XML_Parse(parser: XmlParser, data: *const c_char, len: c_int, is_final: c_int) -> c_int;
    fn XML_SetEncoding(parser: XmlParser, encoding: *const c_char) -> c_int;
    fn XML_SetBase(parser: XmlParser, base: *const c_char) -> c_int;
    fn XML_GetBase(parser: XmlParser) -> *const c_char;
    fn XML_SetParamEntityParsing(parser: XmlParser, setting: c_int) -> c_int;
    fn XML_UseForeignDTD(parser: XmlParser, use_dtd: u8) -> c_int;
    fn XML_SetReturnNSTriplet(parser: XmlParser, do_nst: c_int);
    fn XML_StopParser(parser: XmlParser, resumable: u8);
    fn XML_SetUserData(parser: XmlParser, user_data: *mut c_void);
    fn XML_GetErrorCode(parser: XmlParser) -> c_int;
    fn XML_ErrorString(code: c_int) -> *const c_char;
    fn XML_GetCurrentLineNumber(parser: XmlParser) -> u64;
    fn XML_GetCurrentColumnNumber(parser: XmlParser) -> u64;
    fn XML_GetCurrentByteIndex(parser: XmlParser) -> i64;
    fn XML_GetSpecifiedAttributeCount(parser: XmlParser) -> c_int;
    fn XML_FreeContentModel(parser: XmlParser, model: *mut XmlContent);
    fn XML_ExpatVersion() -> *const c_char;
    fn XML_ExpatVersionInfo() -> XmlExpatVersion;
    fn XML_GetFeatureList() -> *const XmlFeature;

    fn XML_SetElementHandler(parser: XmlParser, start: StartElementHandler, end: EndElementHandler);
    fn XML_SetCharacterDataHandler(parser: XmlParser, handler: CharacterDataHandler);
    fn XML_SetProcessingInstructionHandler(
        parser: XmlParser,
        handler: ProcessingInstructionHandler,
    );
    fn XML_SetCommentHandler(parser: XmlParser, handler: CommentHandler);
    fn XML_SetCdataSectionHandler(
        parser: XmlParser,
        start: CdataSectionHandler,
        end: CdataSectionHandler,
    );
    fn XML_SetDefaultHandlerExpand(parser: XmlParser, handler: DefaultHandler);
    fn XML_SetDoctypeDeclHandler(
        parser: XmlParser,
        start: StartDoctypeHandler,
        end: EndDoctypeHandler,
    );
    fn XML_SetEntityDeclHandler(parser: XmlParser, handler: EntityDeclHandler);
    fn XML_SetUnparsedEntityDeclHandler(parser: XmlParser, handler: UnparsedEntityDeclHandler);
    fn XML_SetNotationDeclHandler(parser: XmlParser, handler: NotationDeclHandler);
    fn XML_SetNamespaceDeclHandler(
        parser: XmlParser,
        start: StartNamespaceDeclHandler,
        end: EndNamespaceDeclHandler,
    );
    fn XML_SetNotStandaloneHandler(parser: XmlParser, handler: NotStandaloneHandler);
    fn XML_SetExternalEntityRefHandler(parser: XmlParser, handler: ExternalEntityRefHandler);
    fn XML_SetXmlDeclHandler(parser: XmlParser, handler: XmlDeclHandler);
    fn XML_SetElementDeclHandler(parser: XmlParser, handler: ElementDeclHandler);
    fn XML_SetAttlistDeclHandler(parser: XmlParser, handler: AttlistDeclHandler);
    fn XML_SetSkippedEntityHandler(parser: XmlParser, handler: SkippedEntityHandler);
}

// Expat's enum values, spelled out because the FFI surface needs the numeric
// constants rather than the C enum type.
const XML_STATUS_OK: c_int = 1;
const XML_ERROR_NONE: c_int = 0;

const XML_PARAM_ENTITY_PARSING_NEVER: i64 = 0;
const XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE: i64 = 1;
const XML_PARAM_ENTITY_PARSING_ALWAYS: i64 = 2;

// `xml.parsers.expat.model` constants.
const XML_CTYPE_EMPTY: i64 = 1;
const XML_CTYPE_ANY: i64 = 2;
const XML_CTYPE_MIXED: i64 = 3;
const XML_CTYPE_NAME: i64 = 4;
const XML_CTYPE_CHOICE: i64 = 5;
const XML_CTYPE_SEQ: i64 = 6;

const XML_CQUANT_NONE: i64 = 0;
const XML_CQUANT_OPT: i64 = 1;
const XML_CQUANT_REP: i64 = 2;
const XML_CQUANT_PLUS: i64 = 3;

const MODULE: &str = "pyexpat";
const HANDLE_ATTR: &str = "_pyexpat_handle";
const INTERN_ATTR: &str = "intern";
const BUFFER_TEXT_ATTR: &str = "_pyexpat_buffer_text";
const BUFFER_SIZE_ATTR: &str = "_pyexpat_buffer_size";
const NAMESPACE_PREFIXES_ATTR: &str = "_pyexpat_namespace_prefixes";
const ORDERED_ATTRIBUTES_ATTR: &str = "_pyexpat_ordered_attributes";
const SPECIFIED_ATTRIBUTES_ATTR: &str = "_pyexpat_specified_attributes";

const DEFAULT_BUFFER_SIZE: i64 = 8192;
const MAX_CHUNK_SIZE: usize = 1 << 20;
const READ_CHUNK_SIZE: i64 = 2048;

/// Every handler attribute CPython exposes. The class namespace gives each an
/// initial `None`, so reading an unset handler matches CPython without an
/// instance attribute.
const HANDLERS: [&str; 22] = [
    "StartElementHandler",
    "EndElementHandler",
    "CharacterDataHandler",
    "ProcessingInstructionHandler",
    "UnparsedEntityDeclHandler",
    "NotationDeclHandler",
    "StartNamespaceDeclHandler",
    "EndNamespaceDeclHandler",
    "CommentHandler",
    "StartCdataSectionHandler",
    "EndCdataSectionHandler",
    "DefaultHandler",
    "DefaultHandlerExpand",
    "NotStandaloneHandler",
    "ExternalEntityRefHandler",
    "StartDoctypeDeclHandler",
    "EndDoctypeDeclHandler",
    "EntityDeclHandler",
    "XmlDeclHandler",
    "ElementDeclHandler",
    "AttlistDeclHandler",
    "SkippedEntityHandler",
];

/// Expat error names by code. Index 0 (`XML_ERROR_NONE`) is not exposed, just
/// like CPython's `errors` module.
const ERROR_NAMES: [&str; 45] = [
    "",
    "XML_ERROR_NO_MEMORY",
    "XML_ERROR_SYNTAX",
    "XML_ERROR_NO_ELEMENTS",
    "XML_ERROR_INVALID_TOKEN",
    "XML_ERROR_UNCLOSED_TOKEN",
    "XML_ERROR_PARTIAL_CHAR",
    "XML_ERROR_TAG_MISMATCH",
    "XML_ERROR_DUPLICATE_ATTRIBUTE",
    "XML_ERROR_JUNK_AFTER_DOC_ELEMENT",
    "XML_ERROR_PARAM_ENTITY_REF",
    "XML_ERROR_UNDEFINED_ENTITY",
    "XML_ERROR_RECURSIVE_ENTITY_REF",
    "XML_ERROR_ASYNC_ENTITY",
    "XML_ERROR_BAD_CHAR_REF",
    "XML_ERROR_BINARY_ENTITY_REF",
    "XML_ERROR_ATTRIBUTE_EXTERNAL_ENTITY_REF",
    "XML_ERROR_MISPLACED_XML_PI",
    "XML_ERROR_UNKNOWN_ENCODING",
    "XML_ERROR_INCORRECT_ENCODING",
    "XML_ERROR_UNCLOSED_CDATA_SECTION",
    "XML_ERROR_EXTERNAL_ENTITY_HANDLING",
    "XML_ERROR_NOT_STANDALONE",
    "XML_ERROR_UNEXPECTED_STATE",
    "XML_ERROR_ENTITY_DECLARED_IN_PE",
    "XML_ERROR_FEATURE_REQUIRES_XML_DTD",
    "XML_ERROR_CANT_CHANGE_FEATURE_ONCE_PARSING",
    "XML_ERROR_UNBOUND_PREFIX",
    "XML_ERROR_UNDECLARING_PREFIX",
    "XML_ERROR_INCOMPLETE_PE",
    "XML_ERROR_XML_DECL",
    "XML_ERROR_TEXT_DECL",
    "XML_ERROR_PUBLICID",
    "XML_ERROR_SUSPENDED",
    "XML_ERROR_NOT_SUSPENDED",
    "XML_ERROR_ABORTED",
    "XML_ERROR_FINISHED",
    "XML_ERROR_SUSPEND_PE",
    "XML_ERROR_RESERVED_PREFIX_XML",
    "XML_ERROR_RESERVED_PREFIX_XMLNS",
    "XML_ERROR_RESERVED_NAMESPACE_URI",
    "XML_ERROR_INVALID_ARGUMENT",
    "XML_ERROR_NO_BUFFER",
    "XML_ERROR_AMPLIFICATION_LIMIT_BREACH",
    "XML_ERROR_NOT_STARTED",
];

// -------------------------------------------------------------------------
// Parser registry
// -------------------------------------------------------------------------

struct ParserEntry {
    /// Raw `XML_Parser` address, stored as an integer so the registry (and
    /// the static `Mutex` around it) stays `Send`: `*mut c_void` is not
    /// `Send`, and an `unsafe impl Send` would hide that requirement from the
    /// compiler. Access to the Expat parser is serialized by the registry
    /// mutex for insertion/lookup only; a parser is used by one Hydro thread
    /// at a time (the instance owning it), and Expat parsers are not shared
    /// across threads by this module.
    parser: usize,
    instance: i64,
    /// Buffered character data when `buffer_text` is enabled.
    pending: Vec<u8>,
}

static REGISTRY: LazyLock<Mutex<HashMap<u64, ParserEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn registry() -> MutexGuard<'static, HashMap<u64, ParserEntry>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn parser_pointer(handle: u64) -> Option<XmlParser> {
    registry()
        .get(&handle)
        .map(|entry| entry.parser as XmlParser)
}

fn parser_parts(handle: u64) -> Option<(XmlParser, i64)> {
    registry()
        .get(&handle)
        .map(|entry| (entry.parser as XmlParser, entry.instance))
}

fn take_pending(handle: u64) -> Vec<u8> {
    let mut table = registry();
    match table.get_mut(&handle) {
        Some(entry) => std::mem::take(&mut entry.pending),
        None => Vec::new(),
    }
}

fn pending_len(handle: u64) -> usize {
    registry()
        .get(&handle)
        .map(|entry| entry.pending.len())
        .unwrap_or(0)
}

fn append_pending(handle: u64, bytes: &[u8]) {
    if let Some(entry) = registry().get_mut(&handle) {
        entry.pending.extend_from_slice(bytes);
    }
}

fn stop_parser(handle: u64) {
    if let Some(parser) = parser_pointer(handle) {
        // SAFETY: `parser` is a live Expat parser owned by the registry.
        unsafe { XML_StopParser(parser, 0) };
    }
}

// -------------------------------------------------------------------------
// Small value helpers
// -------------------------------------------------------------------------

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn value_error(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

fn runtime_error(message: &str) -> i64 {
    exc::raise_with_message(RUNTIME_ERROR, message)
}

fn memory_error() -> i64 {
    exc::raise_with_message(MEMORY_ERROR, "out of memory")
}

fn missing_attribute(name: &str) -> i64 {
    exc::raise_with_message(
        ATTRIBUTE_ERROR,
        &format!("module '{MODULE}' has no attribute '{name}'"),
    )
}

fn int_value(number: i64) -> i64 {
    value::encode_int(number).unwrap_or(NONE)
}

fn bool_value(flag: bool) -> i64 {
    value::bool_value(flag)
}

fn argument(args: &[i64], index: usize) -> i64 {
    args.get(index).copied().unwrap_or(NONE)
}

fn as_c_string(text: &str) -> Result<CString, i64> {
    CString::new(text).map_err(|_| value_error("embedded null character"))
}

unsafe fn c_str(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
}

unsafe fn c_str_or_none(pointer: *const c_char) -> i64 {
    match c_str(pointer) {
        Some(text) => value::str_alloc(text),
        None => NONE,
    }
}

unsafe fn c_str_required(pointer: *const c_char) -> i64 {
    value::str_alloc(c_str(pointer).unwrap_or_default())
}

fn handle_for(instance: i64) -> Result<u64, i64> {
    let Some(raw) = value::instance_attr(instance, HANDLE_ATTR) else {
        return Err(runtime_error("parser is not initialized"));
    };
    let Some(number) = value::as_integer(raw) else {
        return Err(runtime_error("parser is not initialized"));
    };
    if number <= 0 {
        return Err(runtime_error("parser is not initialized"));
    }
    Ok(number as u64)
}

// -------------------------------------------------------------------------
// Rooted argument builder
// -------------------------------------------------------------------------

/// A Python argument vector kept visible to the collector while it is built
/// and while the handler runs.
struct Args {
    values: Vec<i64>,
    roots: usize,
}

impl Args {
    fn new() -> Args {
        Args {
            values: Vec::new(),
            roots: usize::MAX,
        }
    }

    fn push(&mut self, item: i64) {
        self.release();
        self.values.push(item);
        self.roots = heap::root_values(&self.values);
    }

    fn release(&mut self) {
        if self.roots != usize::MAX {
            heap::release_roots(self.roots);
            self.roots = usize::MAX;
        }
    }

    fn pointer(&self) -> *const i64 {
        if self.values.is_empty() {
            std::ptr::null()
        } else {
            self.values.as_ptr()
        }
    }
}

impl Drop for Args {
    fn drop(&mut self) {
        self.release();
    }
}

/// Keeps a small group of values rooted until the guard is dropped.
struct RootGuard(usize);

impl RootGuard {
    fn new(values: &[i64]) -> RootGuard {
        RootGuard(heap::root_values(values))
    }
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        heap::release_roots(self.0);
    }
}

// -------------------------------------------------------------------------
// Interning
// -------------------------------------------------------------------------

fn intern_text(handle: u64, text: String) -> i64 {
    let Some((_, instance)) = parser_parts(handle) else {
        return value::str_alloc(text);
    };
    let Some(intern) = value::instance_attr(instance, INTERN_ATTR) else {
        return value::str_alloc(text);
    };
    let Some(pairs) = value::as_dict(intern) else {
        return value::str_alloc(text);
    };
    for (key, _) in pairs {
        if value::as_str(*key) == Some(text.as_str()) {
            return *key;
        }
    }
    let interned = value::str_alloc(text);
    container::dict_set(intern, interned, interned);
    interned
}

unsafe fn intern_c_str(handle: u64, pointer: *const c_char) -> i64 {
    match c_str(pointer) {
        Some(text) => intern_text(handle, text),
        None => NONE,
    }
}

// -------------------------------------------------------------------------
// Handler invocation
// -------------------------------------------------------------------------

fn handler_value(handle: u64, name: &str) -> Option<i64> {
    let (_, instance) = parser_parts(handle)?;
    if exc::pending() {
        return None;
    }
    let handler = methods::getattr(instance, name.as_ptr(), name.len());
    if exc::pending() {
        // The class namespace always supplies a `None` default for every
        // handler, so this is unreachable in practice; drop the diagnostic
        // rather than failing the parse.
        exc::clear();
        return None;
    }
    if handler == NONE {
        None
    } else {
        Some(handler)
    }
}

fn invoke_func(handle: u64, function: i64, args: &Args) {
    #[cfg(test)]
    if record_test_handler(function, args) {
        return;
    }
    let _ = builtins::call(function, args.values.len(), args.pointer());
    if exc::pending() {
        stop_parser(handle);
    }
}

/// Checks that `name` has a handler, flushes buffered character data first
/// (CPython's handler trampoline does the same), and returns the handler.
fn prepare(handle: u64, name: &str) -> Option<i64> {
    if exc::pending() {
        return None;
    }
    let function = handler_value(handle, name)?;
    flush_pending(handle);
    if exc::pending() {
        return None;
    }
    Some(function)
}

fn invoke_character(handle: u64, function: i64, bytes: &[u8]) {
    let text = value::str_alloc(String::from_utf8_lossy(bytes).into_owned());
    let mut args = Args::new();
    args.push(text);
    invoke_func(handle, function, &args);
}

fn flush_pending(handle: u64) {
    let bytes = take_pending(handle);
    if bytes.is_empty() {
        return;
    }
    let Some(function) = handler_value(handle, "CharacterDataHandler") else {
        return;
    };
    invoke_character(handle, function, &bytes);
}

// -------------------------------------------------------------------------
// Parser configuration helpers
// -------------------------------------------------------------------------

fn hidden(instance: i64, name: &str) -> Option<i64> {
    value::instance_attr(instance, name)
}

fn flag(handle: u64, name: &str, default: bool) -> bool {
    let Some((_, instance)) = parser_parts(handle) else {
        return default;
    };
    match hidden(instance, name) {
        Some(value_arg) => value::truthy(value_arg),
        None => default,
    }
}

fn buffer_text(handle: u64) -> bool {
    flag(handle, BUFFER_TEXT_ATTR, false)
}

fn buffer_size(handle: u64) -> usize {
    let Some((_, instance)) = parser_parts(handle) else {
        return DEFAULT_BUFFER_SIZE as usize;
    };
    match hidden(instance, BUFFER_SIZE_ATTR).and_then(value::as_integer) {
        Some(size) if size > 0 => size as usize,
        _ => DEFAULT_BUFFER_SIZE as usize,
    }
}

fn handle_from_user_data(data: *mut c_void) -> Option<u64> {
    let number = data as usize;
    if number == 0 {
        None
    } else {
        Some(number as u64)
    }
}

/// `XML_GetUserData(parser)` from `expat.h`, which is a macro rather than an
/// exported symbol: `(*(void **)(parser))`. The first member of Expat's
/// `XML_ParserStruct` is the user-data slot written by `XML_SetUserData`, and
/// this ABI layout is what the header itself relies on. `parser` is always the
/// live parser passed by Expat to a callback.
unsafe fn parser_user_data(parser: XmlParser) -> *mut c_void {
    *(parser as *const *mut c_void)
}

// -------------------------------------------------------------------------
// Expat callbacks
// -------------------------------------------------------------------------

unsafe extern "C" fn on_start_element(
    user_data: *mut c_void,
    name: *const c_char,
    attributes: *const *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "StartElementHandler") else {
        return;
    };
    let ordered = flag(handle, ORDERED_ATTRIBUTES_ATTR, false);
    let specified = flag(handle, SPECIFIED_ATTRIBUTES_ATTR, false);
    let mut counted = 0usize;
    if !attributes.is_null() {
        while !(*attributes.add(counted)).is_null() {
            counted += 1;
        }
    }
    let mut limit = counted;
    if specified {
        if let Some(parser) = parser_pointer(handle) {
            let count = XML_GetSpecifiedAttributeCount(parser);
            if count > 0 {
                limit = (count as usize).min(counted);
            }
        }
    }
    let element = intern_c_str(handle, name);
    let mut args = Args::new();
    args.push(element);
    let container = if ordered {
        value::list_new(Vec::new())
    } else {
        value::dict_new(Vec::new())
    };
    args.push(container);
    let mut index = 0usize;
    while index + 1 < limit {
        let attribute = intern_c_str(handle, *attributes.add(index));
        let mut pair = Args::new();
        pair.push(attribute);
        let attribute_value = c_str_or_none(*attributes.add(index + 1));
        pair.push(attribute_value);
        if ordered {
            container::list_append(container, attribute);
            container::list_append(container, attribute_value);
        } else {
            container::dict_set(container, attribute, attribute_value);
        }
        index += 2;
    }
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_end_element(user_data: *mut c_void, name: *const c_char) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "EndElementHandler") else {
        return;
    };
    let element = intern_c_str(handle, name);
    let mut args = Args::new();
    args.push(element);
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_character_data(user_data: *mut c_void, data: *const c_char, len: c_int) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    if exc::pending() || data.is_null() || len < 0 {
        return;
    }
    let Some(function) = handler_value(handle, "CharacterDataHandler") else {
        return;
    };
    let bytes = std::slice::from_raw_parts(data as *const u8, len as usize);
    if !buffer_text(handle) {
        invoke_character(handle, function, bytes);
        return;
    }
    let size = buffer_size(handle);
    let used = pending_len(handle);
    if used > 0 && used + bytes.len() > size {
        flush_pending(handle);
        if exc::pending() {
            return;
        }
    }
    if bytes.len() > size {
        invoke_character(handle, function, bytes);
    } else if !bytes.is_empty() {
        append_pending(handle, bytes);
    }
}

unsafe extern "C" fn on_start_namespace_decl(
    user_data: *mut c_void,
    prefix: *const c_char,
    uri: *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "StartNamespaceDeclHandler") else {
        return;
    };
    let prefix = match c_str(prefix) {
        Some(text) => intern_text(handle, text),
        None => NONE,
    };
    let mut args = Args::new();
    args.push(prefix);
    let uri = c_str_or_none(uri);
    args.push(uri);
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_end_namespace_decl(user_data: *mut c_void, prefix: *const c_char) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "EndNamespaceDeclHandler") else {
        return;
    };
    let prefix = match c_str(prefix) {
        Some(text) => intern_text(handle, text),
        None => NONE,
    };
    let mut args = Args::new();
    args.push(prefix);
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_xml_decl(
    user_data: *mut c_void,
    version: *const c_char,
    encoding: *const c_char,
    standalone: c_int,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "XmlDeclHandler") else {
        return;
    };
    let version = c_str_or_none(version);
    let mut args = Args::new();
    args.push(version);
    let encoding = c_str_or_none(encoding);
    args.push(encoding);
    args.push(int_value(standalone as i64));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_start_doctype(
    user_data: *mut c_void,
    doctype_name: *const c_char,
    system_id: *const c_char,
    public_id: *const c_char,
    has_internal_subset: c_int,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "StartDoctypeDeclHandler") else {
        return;
    };
    let name = intern_c_str(handle, doctype_name);
    let mut args = Args::new();
    args.push(name);
    let system_id = c_str_or_none(system_id);
    args.push(system_id);
    let public_id = c_str_or_none(public_id);
    args.push(public_id);
    args.push(bool_value(has_internal_subset != 0));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_end_doctype(user_data: *mut c_void) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "EndDoctypeDeclHandler") else {
        return;
    };
    let args = Args::new();
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_entity_decl(
    user_data: *mut c_void,
    entity_name: *const c_char,
    is_parameter_entity: c_int,
    value_arg: *const c_char,
    value_length: c_int,
    base: *const c_char,
    system_id: *const c_char,
    public_id: *const c_char,
    notation_name: *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "EntityDeclHandler") else {
        return;
    };
    let name = intern_c_str(handle, entity_name);
    let mut args = Args::new();
    args.push(name);
    args.push(bool_value(is_parameter_entity != 0));
    let value_arg = if value_arg.is_null() || value_length < 0 {
        NONE
    } else {
        let bytes = std::slice::from_raw_parts(value_arg as *const u8, value_length as usize);
        value::str_alloc(String::from_utf8_lossy(bytes).into_owned())
    };
    args.push(value_arg);
    let base = c_str_or_none(base);
    args.push(base);
    let system_id = c_str_or_none(system_id);
    args.push(system_id);
    let public_id = c_str_or_none(public_id);
    args.push(public_id);
    let notation_name = c_str_or_none(notation_name);
    args.push(notation_name);
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_unparsed_entity_decl(
    user_data: *mut c_void,
    entity_name: *const c_char,
    base: *const c_char,
    system_id: *const c_char,
    public_id: *const c_char,
    notation_name: *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "UnparsedEntityDeclHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(intern_c_str(handle, entity_name));
    args.push(c_str_or_none(base));
    args.push(c_str_or_none(system_id));
    args.push(c_str_or_none(public_id));
    args.push(c_str_or_none(notation_name));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_notation_decl(
    user_data: *mut c_void,
    notation_name: *const c_char,
    base: *const c_char,
    system_id: *const c_char,
    public_id: *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "NotationDeclHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(intern_c_str(handle, notation_name));
    args.push(c_str_or_none(base));
    args.push(c_str_or_none(system_id));
    args.push(c_str_or_none(public_id));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_comment(user_data: *mut c_void, data: *const c_char) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "CommentHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(c_str_or_none(data));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_processing_instruction(
    user_data: *mut c_void,
    target: *const c_char,
    data: *const c_char,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "ProcessingInstructionHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(intern_c_str(handle, target));
    args.push(c_str_or_none(data));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_default(user_data: *mut c_void, data: *const c_char, len: c_int) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    // Expat calls the one default handler installed; prefer the regular
    // spelling, falling back to the expand variant.
    let Some(function) = handler_value(handle, "DefaultHandler")
        .or_else(|| handler_value(handle, "DefaultHandlerExpand"))
    else {
        return;
    };
    flush_pending(handle);
    if exc::pending() || data.is_null() || len < 0 {
        return;
    }
    let bytes = std::slice::from_raw_parts(data as *const u8, len as usize);
    invoke_character(handle, function, bytes);
}

unsafe extern "C" fn on_start_cdata(user_data: *mut c_void) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "StartCdataSectionHandler") else {
        return;
    };
    let args = Args::new();
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_end_cdata(user_data: *mut c_void) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "EndCdataSectionHandler") else {
        return;
    };
    let args = Args::new();
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_not_standalone(user_data: *mut c_void) -> c_int {
    let Some(handle) = handle_from_user_data(user_data) else {
        return 1;
    };
    let Some(function) = prepare(handle, "NotStandaloneHandler") else {
        return 1;
    };
    let result = builtins::call(function, 0, std::ptr::null());
    if exc::pending() {
        stop_parser(handle);
        return 0;
    }
    if value::truthy(result) {
        1
    } else {
        0
    }
}

unsafe extern "C" fn on_external_entity_ref(
    parser: XmlParser,
    context: *const c_char,
    base: *const c_char,
    system_id: *const c_char,
    public_id: *const c_char,
) -> c_int {
    if parser.is_null() {
        return 1;
    }
    let user_data = parser_user_data(parser);
    let Some(handle) = handle_from_user_data(user_data) else {
        return 1;
    };
    let Some(function) = prepare(handle, "ExternalEntityRefHandler") else {
        // No Python handler: leave the entity unparsed, which is what Expat
        // itself does when no handler is installed.
        return 1;
    };
    let mut args = Args::new();
    args.push(c_str_or_none(context));
    args.push(c_str_or_none(base));
    args.push(c_str_or_none(system_id));
    args.push(c_str_or_none(public_id));
    let result = builtins::call(function, args.values.len(), args.pointer());
    if exc::pending() {
        stop_parser(handle);
        return 0;
    }
    match value::as_integer(result) {
        Some(0) => 0,
        Some(_) => 1,
        None if value::truthy(result) => 1,
        None => 0,
    }
}

unsafe extern "C" fn on_skipped_entity(
    user_data: *mut c_void,
    entity_name: *const c_char,
    is_parameter_entity: c_int,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "SkippedEntityHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(intern_c_str(handle, entity_name));
    args.push(bool_value(is_parameter_entity != 0));
    invoke_func(handle, function, &args);
}

unsafe extern "C" fn on_attlist_decl(
    user_data: *mut c_void,
    element_name: *const c_char,
    attribute_name: *const c_char,
    attribute_type: *const c_char,
    default_value: *const c_char,
    is_required: c_int,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    let Some(function) = prepare(handle, "AttlistDeclHandler") else {
        return;
    };
    let mut args = Args::new();
    args.push(intern_c_str(handle, element_name));
    args.push(intern_c_str(handle, attribute_name));
    args.push(c_str_or_none(attribute_type));
    args.push(c_str_or_none(default_value));
    args.push(bool_value(is_required != 0));
    invoke_func(handle, function, &args);
}

unsafe fn content_model(handle: u64, model: *const XmlContent) -> i64 {
    if model.is_null() {
        return NONE;
    }
    let model_ref = &*model;
    let mut children = Args::new();
    if !model_ref.children.is_null() {
        for index in 0..model_ref.num_children as usize {
            let child = content_model(handle, model_ref.children.add(index));
            children.push(child);
        }
    }
    let child_tuple = value::tuple_new(children.values.clone());
    // `parts` roots every field while the remaining names are interned; it is
    // declared after `children`, so its root is released first (LIFO).
    let mut parts = Args::new();
    parts.push(int_value(model_ref.kind as i64));
    parts.push(int_value(model_ref.quant as i64));
    parts.push(child_tuple);
    let name = if model_ref.name.is_null() {
        NONE
    } else {
        intern_c_str(handle, model_ref.name as *const c_char)
    };
    parts.push(name);
    value::tuple_new(parts.values.clone())
}

unsafe extern "C" fn on_element_decl(
    user_data: *mut c_void,
    name: *const c_char,
    model: *mut XmlContent,
) {
    let Some(handle) = handle_from_user_data(user_data) else {
        return;
    };
    if let Some(function) = prepare(handle, "ElementDeclHandler") {
        let element = intern_c_str(handle, name);
        let mut args = Args::new();
        args.push(element);
        let content = content_model(handle, model);
        args.push(content);
        invoke_func(handle, function, &args);
    }
    if let Some(parser) = parser_pointer(handle) {
        XML_FreeContentModel(parser, model);
    }
}

/// Installs every callback on a freshly created Expat parser. Expat only calls
/// a C callback for the events it is given, so a Python handler that is unset
/// simply leaves the corresponding callback a no-op.
unsafe fn install_handlers(parser: XmlParser) {
    XML_SetElementHandler(parser, on_start_element, on_end_element);
    XML_SetCharacterDataHandler(parser, on_character_data);
    XML_SetProcessingInstructionHandler(parser, on_processing_instruction);
    XML_SetCommentHandler(parser, on_comment);
    XML_SetCdataSectionHandler(parser, on_start_cdata, on_end_cdata);
    XML_SetDefaultHandlerExpand(parser, on_default);
    XML_SetDoctypeDeclHandler(parser, on_start_doctype, on_end_doctype);
    XML_SetEntityDeclHandler(parser, on_entity_decl);
    XML_SetUnparsedEntityDeclHandler(parser, on_unparsed_entity_decl);
    XML_SetNotationDeclHandler(parser, on_notation_decl);
    XML_SetNamespaceDeclHandler(parser, on_start_namespace_decl, on_end_namespace_decl);
    XML_SetNotStandaloneHandler(parser, on_not_standalone);
    XML_SetExternalEntityRefHandler(parser, on_external_entity_ref);
    XML_SetXmlDeclHandler(parser, on_xml_decl);
    XML_SetElementDeclHandler(parser, on_element_decl);
    XML_SetAttlistDeclHandler(parser, on_attlist_decl);
    XML_SetSkippedEntityHandler(parser, on_skipped_entity);
}

// -------------------------------------------------------------------------
// Error surface
// -------------------------------------------------------------------------

fn cached_class(key: &str, build: impl FnOnce() -> i64) -> i64 {
    if let Some(class) = globals::registry_get(key) {
        return class;
    }
    let class = build();
    globals::registry_set(key, class);
    class
}

fn error_class() -> i64 {
    cached_class("pyexpat.ExpatError", || {
        value::class_new(
            "ExpatError".to_string(),
            exc::exception_class(EXCEPTION),
            Vec::new(),
        )
    })
}

fn error_text(code: c_int) -> String {
    // SAFETY: `XML_ErrorString` returns a static NUL-terminated string or NULL.
    match unsafe { XML_ErrorString(code) } {
        pointer if pointer.is_null() => "unknown error".to_string(),
        pointer => unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    }
}

fn raise_expat_error_code(handle: u64, code: c_int) -> i64 {
    let (line, column) = parser_pointer(handle)
        .map(|parser| {
            // SAFETY: live parser owned by the registry.
            unsafe {
                (
                    XML_GetCurrentLineNumber(parser),
                    XML_GetCurrentColumnNumber(parser),
                )
            }
        })
        .unwrap_or((0, 0));
    let message = format!("{}: line {}, column {}", error_text(code), line, column);
    let argument = value::str_alloc(message);
    let roots = heap::root_values(&[argument]);
    let class = error_class();
    let error = builtins::call(class, 1, [argument].as_ptr());
    if exc::pending() {
        heap::release_roots(roots);
        return NONE;
    }
    value::instance_setattr(error, "code".to_string(), int_value(code as i64));
    value::instance_setattr(error, "lineno".to_string(), int_value(line as i64));
    value::instance_setattr(error, "offset".to_string(), int_value(column as i64));
    heap::release_roots(roots);
    exc::raise_value(error)
}

fn raise_expat_error(handle: u64) -> i64 {
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    // SAFETY: live parser.
    let code = unsafe { XML_GetErrorCode(parser) };
    raise_expat_error_code(handle, code)
}

fn errors_constant(name: &str) -> Option<i64> {
    let index = ERROR_NAMES
        .iter()
        .position(|candidate| *candidate == name)?;
    Some(value::str_alloc(error_text(index as c_int)))
}

fn error_codes() -> i64 {
    cached_class("pyexpat.errors.codes", || {
        let dict = value::dict_new(Vec::new());
        let roots = heap::root_values(&[dict]);
        for (code, name) in ERROR_NAMES.iter().enumerate().skip(1) {
            if name.is_empty() {
                continue;
            }
            let message = value::str_alloc(error_text(code as c_int));
            container::dict_set(dict, message, int_value(code as i64));
        }
        heap::release_roots(roots);
        dict
    })
}

fn error_messages() -> i64 {
    cached_class("pyexpat.errors.messages", || {
        let dict = value::dict_new(Vec::new());
        let roots = heap::root_values(&[dict]);
        for (code, name) in ERROR_NAMES.iter().enumerate().skip(1) {
            if name.is_empty() {
                continue;
            }
            let message = value::str_alloc(error_text(code as c_int));
            container::dict_set(dict, int_value(code as i64), message);
        }
        heap::release_roots(roots);
        dict
    })
}

/// The `pyexpat.errors` object. CPython publishes it as a real submodule; the
/// runtime has no per-module static table for it, so it is a cached class
/// whose namespace carries the error-name strings and the two mappings.
fn errors_object() -> i64 {
    cached_class("pyexpat.errors", || {
        let codes = error_codes();
        let messages = error_messages();
        let mut kept = Args::new();
        kept.push(codes);
        kept.push(messages);
        let mut attributes: Vec<(String, i64)> = Vec::new();
        for (code, name) in ERROR_NAMES.iter().enumerate().skip(1) {
            if name.is_empty() {
                continue;
            }
            let message = value::str_alloc(error_text(code as c_int));
            attributes.push(((*name).to_string(), message));
            kept.push(message);
        }
        attributes.push(("codes".to_string(), codes));
        attributes.push(("messages".to_string(), messages));
        value::class_new_full("errors".to_string(), NONE, Vec::new(), attributes)
    })
}

fn model_object() -> i64 {
    cached_class("pyexpat.model", || {
        let attributes = vec![
            ("XML_CTYPE_EMPTY".to_string(), int_value(XML_CTYPE_EMPTY)),
            ("XML_CTYPE_ANY".to_string(), int_value(XML_CTYPE_ANY)),
            ("XML_CTYPE_MIXED".to_string(), int_value(XML_CTYPE_MIXED)),
            ("XML_CTYPE_NAME".to_string(), int_value(XML_CTYPE_NAME)),
            ("XML_CTYPE_CHOICE".to_string(), int_value(XML_CTYPE_CHOICE)),
            ("XML_CTYPE_SEQ".to_string(), int_value(XML_CTYPE_SEQ)),
            ("XML_CQUANT_NONE".to_string(), int_value(XML_CQUANT_NONE)),
            ("XML_CQUANT_OPT".to_string(), int_value(XML_CQUANT_OPT)),
            ("XML_CQUANT_REP".to_string(), int_value(XML_CQUANT_REP)),
            ("XML_CQUANT_PLUS".to_string(), int_value(XML_CQUANT_PLUS)),
        ];
        value::class_new_full("model".to_string(), NONE, Vec::new(), attributes)
    })
}

// -------------------------------------------------------------------------
// Parser class and methods
// -------------------------------------------------------------------------

fn native(name: &'static str) -> i64 {
    value::native_function_new(MODULE, name, usize::MAX)
}

fn bool_property(getter: &'static str, setter: &'static str) -> i64 {
    value::property_new(native(getter), native(setter), NONE)
}

fn read_property(getter: &'static str) -> i64 {
    value::property_new(native(getter), NONE, NONE)
}

fn parser_class() -> i64 {
    cached_class("pyexpat.XMLParserType", || {
        let methods = vec![
            (
                "__init__".to_string(),
                native("_pyexpat_init"),
                MethodKind::Normal,
            ),
            (
                "Parse".to_string(),
                native("_pyexpat_parse"),
                MethodKind::Normal,
            ),
            (
                "ParseFile".to_string(),
                native("_pyexpat_parse_file"),
                MethodKind::Normal,
            ),
            (
                "SetBase".to_string(),
                native("_pyexpat_set_base"),
                MethodKind::Normal,
            ),
            (
                "GetBase".to_string(),
                native("_pyexpat_get_base"),
                MethodKind::Normal,
            ),
            (
                "ExternalEntityParserCreate".to_string(),
                native("_pyexpat_external_entity_parser_create"),
                MethodKind::Normal,
            ),
            (
                "SetParamEntityParsing".to_string(),
                native("_pyexpat_set_param_entity_parsing"),
                MethodKind::Normal,
            ),
            (
                "UseForeignDTD".to_string(),
                native("_pyexpat_use_foreign_dtd"),
                MethodKind::Normal,
            ),
        ];
        let mut attributes: Vec<(String, i64)> = Vec::new();
        for handler in HANDLERS {
            attributes.push((handler.to_string(), NONE));
        }
        attributes.push((
            "buffer_text".to_string(),
            bool_property("_pyexpat_get_buffer_text", "_pyexpat_set_buffer_text"),
        ));
        attributes.push((
            "buffer_size".to_string(),
            bool_property("_pyexpat_get_buffer_size", "_pyexpat_set_buffer_size"),
        ));
        attributes.push((
            "buffer_used".to_string(),
            read_property("_pyexpat_get_buffer_used"),
        ));
        attributes.push((
            "namespace_prefixes".to_string(),
            bool_property(
                "_pyexpat_get_namespace_prefixes",
                "_pyexpat_set_namespace_prefixes",
            ),
        ));
        attributes.push((
            "ordered_attributes".to_string(),
            bool_property(
                "_pyexpat_get_ordered_attributes",
                "_pyexpat_set_ordered_attributes",
            ),
        ));
        attributes.push((
            "specified_attributes".to_string(),
            bool_property(
                "_pyexpat_get_specified_attributes",
                "_pyexpat_set_specified_attributes",
            ),
        ));
        attributes.push((
            "ErrorCode".to_string(),
            read_property("_pyexpat_get_error_code"),
        ));
        attributes.push((
            "ErrorLineNumber".to_string(),
            read_property("_pyexpat_get_current_line"),
        ));
        attributes.push((
            "ErrorColumnNumber".to_string(),
            read_property("_pyexpat_get_current_column"),
        ));
        attributes.push((
            "ErrorByteIndex".to_string(),
            read_property("_pyexpat_get_current_byte"),
        ));
        attributes.push((
            "CurrentLineNumber".to_string(),
            read_property("_pyexpat_get_current_line"),
        ));
        attributes.push((
            "CurrentColumnNumber".to_string(),
            read_property("_pyexpat_get_current_column"),
        ));
        attributes.push((
            "CurrentByteIndex".to_string(),
            read_property("_pyexpat_get_current_byte"),
        ));
        value::class_new_full("xmlparser".to_string(), NONE, methods, attributes)
    })
}

fn register_parser(instance: i64, parser: XmlParser) -> Result<(), i64> {
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
    value::instance_setattr(instance, HANDLE_ATTR.to_string(), int_value(handle as i64));
    // SAFETY: `parser` is a fresh parser owned by the registry; the callback
    // user data is a non-null integer handle that is never dereferenced.
    unsafe {
        XML_SetUserData(parser, handle as usize as *mut c_void);
        install_handlers(parser);
    }
    registry().insert(
        handle,
        ParserEntry {
            parser: parser as usize,
            instance,
            pending: Vec::new(),
        },
    );
    Ok(())
}

fn create_parser(
    encoding: Option<&str>,
    separator: Option<&str>,
    intern: i64,
    existing_instance: Option<i64>,
) -> i64 {
    // The caller may have just allocated the `intern` dictionary; keep it
    // reachable across the instance allocation below. `None` interning is the
    // immediate `None` value, which needs no root.
    let _intern_roots = if intern != NONE {
        Some(RootGuard::new(&[intern]))
    } else {
        None
    };
    let encoding_c = match encoding {
        Some(text) => match as_c_string(text) {
            Ok(text) => Some(text),
            Err(err) => return err,
        },
        None => None,
    };
    let encoding_pointer = encoding_c
        .as_ref()
        .map(|text| text.as_ptr())
        .unwrap_or(std::ptr::null());
    // SAFETY: pointers either point at live `CString`s for the duration of the
    // call or are NULL, as libexpat requires.
    let parser = unsafe {
        match separator {
            None => XML_ParserCreate(encoding_pointer),
            Some(text) => {
                if text.len() > 1 {
                    return value_error(
                        "namespace_separator must be at most one character, omitted, or None",
                    );
                }
                let separator_char = text.as_bytes().first().copied().unwrap_or(0);
                XML_ParserCreateNS(encoding_pointer, separator_char as c_char)
            }
        }
    };
    if parser.is_null() {
        return runtime_error("XML_ParserCreate failed");
    }
    let instance = match existing_instance {
        Some(instance) => instance,
        None => value::instance_new(parser_class()),
    };
    let roots = heap::root_values(&[instance]);
    value::instance_setattr(instance, INTERN_ATTR.to_string(), intern);
    let result = register_parser(instance, parser);
    heap::release_roots(roots);
    match result {
        Ok(()) => instance,
        Err(err) => err,
    }
}

fn parser_create_from_args(args: &[i64]) -> i64 {
    let encoding = match argument(args, 0) {
        NONE => None,
        encoding => match value::as_str(encoding) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "ParserCreate() argument 'encoding' must be str or None, not {}",
                    value::type_name(encoding)
                ))
            }
        },
    };
    let separator = match argument(args, 1) {
        NONE => None,
        separator => match value::as_str(separator) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "ParserCreate() argument 'namespace_separator' must be str or None, not {}",
                    value::type_name(separator)
                ))
            }
        },
    };
    // An explicit third argument distinguishes "no interning" (`None`) from
    // the default "create a new dictionary".
    let intern = if args.len() >= 3 {
        match args[2] {
            NONE => NONE,
            dictionary if value::as_dict(dictionary).is_some() => dictionary,
            _ => return type_error("intern must be a dictionary"),
        }
    } else {
        value::dict_new(Vec::new())
    };
    create_parser(encoding, separator, intern, None)
}

fn parse_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let data = match args.get(1) {
        Some(data) => *data,
        None => return type_error("Parse() missing required argument 'data' (pos 1)"),
    };
    let is_final = args
        .get(2)
        .map(|value_arg| value::truthy(*value_arg))
        .unwrap_or(false);
    let (bytes, text_input) = if let Some(text) = value::as_str(data) {
        (text.as_bytes().to_vec(), true)
    } else if let Some(bytes) = value::as_bytes(data) {
        (bytes.to_vec(), false)
    } else if let Some(view) = value::as_memoryview(data) {
        match value::memoryview_bytes(view) {
            Ok(bytes) => (bytes, false),
            Err(message) => return value_error(message),
        }
    } else {
        return type_error(&format!(
            "Parse() argument 1 must be a bytes-like object, not {}",
            value::type_name(data)
        ));
    };
    if text_input {
        // SAFETY: static NUL-terminated encoding name.
        unsafe { XML_SetEncoding(parser, c"utf-8".as_ptr()) };
    }
    let mut result = XML_STATUS_OK;
    if bytes.is_empty() {
        // SAFETY: zero-length parse accepts a null buffer.
        result = unsafe { XML_Parse(parser, std::ptr::null(), 0, if is_final { 1 } else { 0 }) };
    } else {
        let mut offset = 0usize;
        while offset < bytes.len() {
            let chunk = (bytes.len() - offset).min(MAX_CHUNK_SIZE);
            let final_chunk = is_final && offset + chunk == bytes.len();
            // SAFETY: `offset + chunk <= bytes.len()`, so the pointer is in
            // bounds and the length fits in `c_int`.
            result = unsafe {
                XML_Parse(
                    parser,
                    bytes.as_ptr().add(offset) as *const c_char,
                    chunk as c_int,
                    if final_chunk { 1 } else { 0 },
                )
            };
            if result == 0 {
                break;
            }
            offset += chunk;
        }
    }
    if exc::pending() {
        return NONE;
    }
    if result == 0 {
        return raise_expat_error(handle);
    }
    flush_pending(handle);
    if exc::pending() {
        return NONE;
    }
    int_value(1)
}

fn parse_file_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let file = argument(args, 1);
    let read = methods::getattr(file, b"read".as_ptr(), 4);
    if exc::pending() {
        exc::clear();
        return type_error("argument must have 'read' attribute");
    }
    if read == NONE {
        return type_error("argument must have 'read' attribute");
    }
    let _roots = RootGuard::new(&[file, read]);
    let request = int_value(READ_CHUNK_SIZE);
    let result = loop {
        let chunk = builtins::call(read, 1, [request].as_ptr());
        if exc::pending() {
            return NONE;
        }
        let Some(bytes) = value::as_bytes(chunk) else {
            return type_error(&format!(
                "read() did not return a bytes object (type={})",
                value::type_name(chunk)
            ));
        };
        if bytes.len() > READ_CHUNK_SIZE as usize {
            return value_error(&format!(
                "read() returned too much data: {} bytes requested, {} returned",
                READ_CHUNK_SIZE,
                bytes.len()
            ));
        }
        let final_chunk = bytes.is_empty();
        // SAFETY: the slice is owned by a live Hydro bytes object for the
        // duration of this call; its length is bounded by `READ_CHUNK_SIZE`.
        let status = unsafe {
            XML_Parse(
                parser,
                bytes.as_ptr() as *const c_char,
                bytes.len() as c_int,
                if final_chunk { 1 } else { 0 },
            )
        };
        if exc::pending() {
            return NONE;
        }
        if status == 0 || final_chunk {
            break status;
        }
    };
    if result == 0 {
        return raise_expat_error(handle);
    }
    flush_pending(handle);
    if exc::pending() {
        return NONE;
    }
    int_value(1)
}

fn set_base_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let base = match args.get(1) {
        Some(base) => match value::as_str(*base) {
            Some(text) => text,
            None => {
                return type_error(&format!(
                    "SetBase() argument 1 must be str, not {}",
                    value::type_name(*base)
                ))
            }
        },
        None => return type_error("SetBase() missing required argument 'base' (pos 1)"),
    };
    let base_c = match as_c_string(base) {
        Ok(base) => base,
        Err(err) => return err,
    };
    // SAFETY: `base_c` is a live NUL-terminated string for the call.
    if unsafe { XML_SetBase(parser, base_c.as_ptr()) } == 0 {
        return memory_error();
    }
    NONE
}

fn get_base_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    // SAFETY: live parser; `XML_GetBase` returns a parser-owned string or NULL.
    let base = unsafe { XML_GetBase(parser) };
    unsafe { c_str_or_none(base) }
}

fn external_entity_parser_create(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parent) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let context = match argument(args, 1) {
        NONE => None,
        value_arg => match value::as_str(value_arg) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "ExternalEntityParserCreate() argument 1 must be str or None, not {}",
                    value::type_name(value_arg)
                ))
            }
        },
    };
    let encoding = match argument(args, 2) {
        NONE => None,
        value_arg => match value::as_str(value_arg) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "ExternalEntityParserCreate() argument 2 must be str or None, not {}",
                    value::type_name(value_arg)
                ))
            }
        },
    };
    let context_c = match context {
        Some(text) => match as_c_string(text) {
            Ok(text) => Some(text),
            Err(err) => return err,
        },
        None => None,
    };
    let encoding_c = match encoding {
        Some(text) => match as_c_string(text) {
            Ok(text) => Some(text),
            Err(err) => return err,
        },
        None => None,
    };
    // SAFETY: the parent parser is live; the optional strings live for the
    // duration of the call.
    let parser = unsafe {
        XML_ExternalEntityParserCreate(
            parent,
            context_c
                .as_ref()
                .map(|text| text.as_ptr())
                .unwrap_or(std::ptr::null()),
            encoding_c
                .as_ref()
                .map(|text| text.as_ptr())
                .unwrap_or(std::ptr::null()),
        )
    };
    if parser.is_null() {
        return memory_error();
    }
    let child = value::instance_new(parser_class());
    let roots = heap::root_values(&[child]);
    let intern = value::instance_attr(instance, INTERN_ATTR).unwrap_or(NONE);
    value::instance_setattr(child, INTERN_ATTR.to_string(), intern);
    for handler in HANDLERS {
        if let Some(handler_value) = value::instance_attr(instance, handler) {
            value::instance_setattr(child, handler.to_string(), handler_value);
        }
    }
    for flag_name in [
        BUFFER_TEXT_ATTR,
        BUFFER_SIZE_ATTR,
        NAMESPACE_PREFIXES_ATTR,
        ORDERED_ATTRIBUTES_ATTR,
        SPECIFIED_ATTRIBUTES_ATTR,
    ] {
        if let Some(flag_value) = value::instance_attr(instance, flag_name) {
            value::instance_setattr(child, flag_name.to_string(), flag_value);
        }
    }
    let result = register_parser(child, parser);
    heap::release_roots(roots);
    match result {
        Ok(()) => child,
        Err(err) => err,
    }
}

fn set_param_entity_parsing(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let flag = match args
        .get(1)
        .and_then(|value_arg| value::as_integer(*value_arg))
    {
        Some(flag) => flag,
        None => {
            return type_error(&format!(
                "SetParamEntityParsing() argument 1 must be int, not {}",
                value::type_name(argument(args, 1))
            ))
        }
    };
    // SAFETY: live parser.
    let accepted = unsafe { XML_SetParamEntityParsing(parser, flag as c_int) };
    bool_value(accepted != 0)
}

fn use_foreign_dtd(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let handle = match handle_for(instance) {
        Ok(handle) => handle,
        Err(err) => return err,
    };
    let Some(parser) = parser_pointer(handle) else {
        return runtime_error("parser is not initialized");
    };
    let flag = args
        .get(1)
        .map(|value_arg| value::truthy(*value_arg))
        .unwrap_or(true);
    // SAFETY: live parser.
    let status = unsafe { XML_UseForeignDTD(parser, u8::from(flag)) };
    if status != XML_ERROR_NONE {
        return raise_expat_error_code(handle, status);
    }
    NONE
}

fn init_method(args: &[i64]) -> i64 {
    let instance = argument(args, 0);
    let encoding_arg = argument(args, 1);
    let encoding = match encoding_arg {
        NONE => None,
        value_arg => match value::as_str(value_arg) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "xmlparser() argument 'encoding' must be str or None, not {}",
                    value::type_name(value_arg)
                ))
            }
        },
    };
    let separator_arg = argument(args, 2);
    let separator = match separator_arg {
        NONE => None,
        value_arg => match value::as_str(value_arg) {
            Some(text) => Some(text),
            None => {
                return type_error(&format!(
                    "xmlparser() argument 'namespace_separator' must be str or None, not {}",
                    value::type_name(value_arg)
                ))
            }
        },
    };
    let intern = if args.len() >= 4 {
        match args[3] {
            NONE => NONE,
            dictionary if value::as_dict(dictionary).is_some() => dictionary,
            _ => return type_error("intern must be a dictionary"),
        }
    } else {
        value::dict_new(Vec::new())
    };
    let result = create_parser(encoding, separator, intern, Some(instance));
    if result == instance {
        NONE
    } else {
        result
    }
}

// -------------------------------------------------------------------------
// Property accessors
// -------------------------------------------------------------------------

fn property_instance(args: &[i64]) -> i64 {
    argument(args, 0)
}

fn get_flag_property(args: &[i64], name: &str, default: bool) -> i64 {
    let instance = property_instance(args);
    match hidden(instance, name) {
        Some(value_arg) => bool_value(value::truthy(value_arg)),
        None => bool_value(default),
    }
}

fn set_flag_property(args: &[i64], name: &str) -> i64 {
    let instance = property_instance(args);
    let flag = value::truthy(argument(args, 1));
    value::instance_setattr(instance, name.to_string(), bool_value(flag));
    NONE
}

fn get_buffer_text(args: &[i64]) -> i64 {
    get_flag_property(args, BUFFER_TEXT_ATTR, false)
}

fn set_buffer_text(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let flag = value::truthy(argument(args, 1));
    value::instance_setattr(instance, BUFFER_TEXT_ATTR.to_string(), bool_value(flag));
    if !flag {
        if let Ok(handle) = handle_for(instance) {
            flush_pending(handle);
        }
    }
    NONE
}

fn get_buffer_size(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    match hidden(instance, BUFFER_SIZE_ATTR).and_then(value::as_integer) {
        Some(size) if size > 0 => int_value(size),
        _ => int_value(DEFAULT_BUFFER_SIZE),
    }
}

fn set_buffer_size(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let Some(size) = value::as_integer(argument(args, 1)) else {
        return type_error("buffer_size must be an integer");
    };
    if size <= 0 {
        return value_error("buffer_size must be greater than zero");
    }
    if size > i64::from(i32::MAX) {
        return value_error(&format!(
            "buffer_size must not be greater than {}",
            i32::MAX
        ));
    }
    value::instance_setattr(instance, BUFFER_SIZE_ATTR.to_string(), int_value(size));
    NONE
}

fn get_buffer_used(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    match handle_for(instance) {
        Ok(handle) => int_value(pending_len(handle) as i64),
        Err(_) => int_value(0),
    }
}

fn get_namespace_prefixes(args: &[i64]) -> i64 {
    get_flag_property(args, NAMESPACE_PREFIXES_ATTR, false)
}

fn set_namespace_prefixes(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let flag = value::truthy(argument(args, 1));
    value::instance_setattr(
        instance,
        NAMESPACE_PREFIXES_ATTR.to_string(),
        bool_value(flag),
    );
    if let Ok(handle) = handle_for(instance) {
        if let Some(parser) = parser_pointer(handle) {
            // SAFETY: live parser.
            unsafe { XML_SetReturnNSTriplet(parser, if flag { 1 } else { 0 }) };
        }
    }
    NONE
}

fn get_ordered_attributes(args: &[i64]) -> i64 {
    get_flag_property(args, ORDERED_ATTRIBUTES_ATTR, false)
}

fn set_ordered_attributes(args: &[i64]) -> i64 {
    set_flag_property(args, ORDERED_ATTRIBUTES_ATTR)
}

fn get_specified_attributes(args: &[i64]) -> i64 {
    get_flag_property(args, SPECIFIED_ATTRIBUTES_ATTR, false)
}

fn set_specified_attributes(args: &[i64]) -> i64 {
    set_flag_property(args, SPECIFIED_ATTRIBUTES_ATTR)
}

fn current_code(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let Ok(handle) = handle_for(instance) else {
        return int_value(0);
    };
    match parser_pointer(handle) {
        // SAFETY: live parser.
        Some(parser) => int_value(unsafe { XML_GetErrorCode(parser) } as i64),
        None => int_value(0),
    }
}

fn current_line(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let Ok(handle) = handle_for(instance) else {
        return int_value(0);
    };
    match parser_pointer(handle) {
        // SAFETY: live parser.
        Some(parser) => int_value(unsafe { XML_GetCurrentLineNumber(parser) } as i64),
        None => int_value(0),
    }
}

fn current_column(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let Ok(handle) = handle_for(instance) else {
        return int_value(0);
    };
    match parser_pointer(handle) {
        // SAFETY: live parser.
        Some(parser) => int_value(unsafe { XML_GetCurrentColumnNumber(parser) } as i64),
        None => int_value(0),
    }
}

fn current_byte(args: &[i64]) -> i64 {
    let instance = property_instance(args);
    let Ok(handle) = handle_for(instance) else {
        return int_value(-1);
    };
    match parser_pointer(handle) {
        // SAFETY: live parser.
        Some(parser) => int_value(unsafe { XML_GetCurrentByteIndex(parser) }),
        None => int_value(-1),
    }
}

// -------------------------------------------------------------------------
// Module surface
// -------------------------------------------------------------------------

fn version_info() -> i64 {
    // SAFETY: returns a plain by-value version struct.
    let info = unsafe { XML_ExpatVersionInfo() };
    value::tuple_new(vec![
        int_value(info.major as i64),
        int_value(info.minor as i64),
        int_value(info.micro as i64),
    ])
}

fn expat_version() -> i64 {
    // SAFETY: returns a static NUL-terminated version string.
    unsafe { c_str_required(XML_ExpatVersion()) }
}

fn features() -> i64 {
    cached_class("pyexpat.features", || {
        let list = value::list_new(Vec::new());
        let roots = heap::root_values(&[list]);
        // SAFETY: the feature table is static and terminated by
        // `XML_FEATURE_END` (0).
        let mut pointer = unsafe { XML_GetFeatureList() };
        while !pointer.is_null() {
            let feature = unsafe { *pointer };
            if feature.feature == 0 {
                break;
            }
            let name = if feature.name.is_null() {
                value::str_alloc(String::new())
            } else {
                unsafe { c_str_required(feature.name) }
            };
            let mut pair = Args::new();
            pair.push(name);
            pair.push(int_value(feature.value));
            let item = value::tuple_new(pair.values.clone());
            container::list_append(list, item);
            pointer = unsafe { pointer.add(1) };
        }
        heap::release_roots(roots);
        list
    })
}

/// Materialises a module constant. The same table serves `pyexpat` and the
/// `pyexpat.errors` / `pyexpat.model` spellings so a spec entry for those
/// submodules can delegate here.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "error" | "ExpatError" => error_class(),
        "XMLParserType" => parser_class(),
        "errors" => errors_object(),
        "model" => model_object(),
        "codes" => error_codes(),
        "messages" => error_messages(),
        "EXPAT_VERSION" => expat_version(),
        "version_info" => version_info(),
        "native_encoding" => value::str_alloc("UTF-8".to_string()),
        "features" => features(),
        "XML_PARAM_ENTITY_PARSING_NEVER" => int_value(XML_PARAM_ENTITY_PARSING_NEVER),
        "XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE" => {
            int_value(XML_PARAM_ENTITY_PARSING_UNLESS_STANDALONE)
        }
        "XML_PARAM_ENTITY_PARSING_ALWAYS" => int_value(XML_PARAM_ENTITY_PARSING_ALWAYS),
        "XML_CTYPE_EMPTY" => int_value(XML_CTYPE_EMPTY),
        "XML_CTYPE_ANY" => int_value(XML_CTYPE_ANY),
        "XML_CTYPE_MIXED" => int_value(XML_CTYPE_MIXED),
        "XML_CTYPE_NAME" => int_value(XML_CTYPE_NAME),
        "XML_CTYPE_CHOICE" => int_value(XML_CTYPE_CHOICE),
        "XML_CTYPE_SEQ" => int_value(XML_CTYPE_SEQ),
        "XML_CQUANT_NONE" => int_value(XML_CQUANT_NONE),
        "XML_CQUANT_OPT" => int_value(XML_CQUANT_OPT),
        "XML_CQUANT_REP" => int_value(XML_CQUANT_REP),
        "XML_CQUANT_PLUS" => int_value(XML_CQUANT_PLUS),
        _ => match errors_constant(name) {
            Some(value_arg) => value_arg,
            None => missing_attribute(name),
        },
    }
}

/// Dispatches `pyexpat` module calls and the native bodies installed on
/// `xmlparser`.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "ParserCreate" => parser_create_from_args(args),
        "ErrorString" => {
            let Some(code) = args
                .first()
                .and_then(|value_arg| value::as_integer(*value_arg))
            else {
                return type_error(&format!(
                    "ErrorString() argument 1 must be int, not {}",
                    value::type_name(argument(args, 0))
                ));
            };
            // SAFETY: `XML_ErrorString` accepts any int and returns NULL for
            // out-of-range values.
            let pointer = unsafe { XML_ErrorString(code as c_int) };
            unsafe { c_str_or_none(pointer) }
        }
        "_pyexpat_init" => init_method(args),
        "_pyexpat_parse" => parse_method(args),
        "_pyexpat_parse_file" => parse_file_method(args),
        "_pyexpat_set_base" => set_base_method(args),
        "_pyexpat_get_base" => get_base_method(args),
        "_pyexpat_external_entity_parser_create" => external_entity_parser_create(args),
        "_pyexpat_set_param_entity_parsing" => set_param_entity_parsing(args),
        "_pyexpat_use_foreign_dtd" => use_foreign_dtd(args),
        "_pyexpat_get_buffer_text" => get_buffer_text(args),
        "_pyexpat_set_buffer_text" => set_buffer_text(args),
        "_pyexpat_get_buffer_size" => get_buffer_size(args),
        "_pyexpat_set_buffer_size" => set_buffer_size(args),
        "_pyexpat_get_buffer_used" => get_buffer_used(args),
        "_pyexpat_get_namespace_prefixes" => get_namespace_prefixes(args),
        "_pyexpat_set_namespace_prefixes" => set_namespace_prefixes(args),
        "_pyexpat_get_ordered_attributes" => get_ordered_attributes(args),
        "_pyexpat_set_ordered_attributes" => set_ordered_attributes(args),
        "_pyexpat_get_specified_attributes" => get_specified_attributes(args),
        "_pyexpat_set_specified_attributes" => set_specified_attributes(args),
        "_pyexpat_get_error_code" => current_code(args),
        "_pyexpat_get_current_line" => current_line(args),
        "_pyexpat_get_current_column" => current_column(args),
        "_pyexpat_get_current_byte" => current_byte(args),
        _ => missing_attribute(function),
    }
}

/// Keyword variant for `ParserCreate(encoding=..., namespace_separator=...,
/// intern=...)`. The central binder normally moves declared names into their
/// positional slots before dispatch; this retains CPython's duplicate and
/// unknown-name diagnostics for direct callers.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function != "ParserCreate" {
        return type_error(&format!(
            "{MODULE}.{function}() does not accept keyword arguments"
        ));
    }
    let mut slots: Vec<i64> = args.to_vec();
    for (name, value_arg) in keywords {
        let index = match name.as_str() {
            "encoding" => 0,
            "namespace_separator" => 1,
            "intern" => 2,
            other => {
                return type_error(&format!(
                    "ParserCreate() got an unexpected keyword argument '{other}'"
                ))
            }
        };
        if index < slots.len() {
            return type_error(&format!(
                "ParserCreate() got multiple values for argument '{name}'"
            ));
        }
        while slots.len() < index {
            slots.push(NONE);
        }
        slots.push(*value_arg);
    }
    if slots.len() > 3 {
        return type_error("ParserCreate() takes at most 3 arguments");
    }
    parser_create_from_args(&slots)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Marker native functions that the callback trampoline recognises as test
    // recorders. An `Args` vector is not rooted after the handler returns, so
    // the recorder stores a printable description instead of the raw values.
    fn recorder(name: &'static str) -> i64 {
        value::native_function_new(MODULE, name, usize::MAX)
    }

    fn events() -> Vec<(String, Vec<String>)> {
        TEST_EVENTS.with(|slot| slot.borrow().clone())
    }

    fn clear_events() {
        TEST_EVENTS.with(|slot| slot.borrow_mut().clear());
    }

    fn attach_handler(parser: i64, name: &str, function: i64) {
        value::instance_setattr(parser, name.to_string(), function);
    }

    fn new_parser(separator: Option<&str>) -> i64 {
        match separator {
            Some(separator) => call(
                "ParserCreate",
                &[NONE, value::str_alloc(separator.to_string())],
            ),
            None => call("ParserCreate", &[]),
        }
    }

    fn parse(parser: i64, text: &str) -> i64 {
        call(
            "_pyexpat_parse",
            &[
                parser,
                value::str_alloc(text.to_string()),
                value::bool_value(true),
            ],
        )
    }

    #[test]
    fn parses_simple_document_with_handlers() {
        exc::init();
        clear_events();
        let parser = new_parser(None);
        assert!(!exc::pending());
        attach_handler(
            parser,
            "StartElementHandler",
            recorder("_pyexpat_test_start"),
        );
        attach_handler(parser, "EndElementHandler", recorder("_pyexpat_test_end"));
        attach_handler(
            parser,
            "CharacterDataHandler",
            recorder("_pyexpat_test_char"),
        );
        let result = parse(parser, r#"<root a="1">hi<b/></root>"#);
        assert_eq!(value::as_integer(result), Some(1));
        assert!(!exc::pending());
        let recorded = events();
        assert!(recorded.iter().any(|(name, args)| {
            name == "_pyexpat_test_start" && args.first().map(String::as_str) == Some("s:root")
        }));
        let start = recorded
            .iter()
            .find(|(name, _)| name == "_pyexpat_test_start")
            .expect("start element event");
        assert_eq!(start.1[1], "d{s:a=s:1}");
        assert!(recorded
            .iter()
            .any(|(name, args)| name == "_pyexpat_test_char"
                && args.first().map(String::as_str) == Some("s:hi")));
        assert!(recorded
            .iter()
            .any(|(name, args)| name == "_pyexpat_test_end"
                && args.first().map(String::as_str) == Some("s:root")));
    }

    #[test]
    fn expands_namespaced_names() {
        exc::init();
        clear_events();
        let parser = new_parser(Some(" "));
        assert!(!exc::pending());
        attach_handler(
            parser,
            "StartElementHandler",
            recorder("_pyexpat_test_start"),
        );
        let result = parse(parser, r#"<r xmlns:p="urn:x"><p:c/></r>"#);
        assert_eq!(value::as_integer(result), Some(1));
        assert!(!exc::pending());
        let names: Vec<String> = events()
            .into_iter()
            .filter(|(name, _)| name == "_pyexpat_test_start")
            .map(|(_, args)| args[0].clone())
            .collect();
        assert_eq!(names, vec!["s:r", "s:urn:x c"]);
    }

    #[test]
    fn malformed_document_raises_expat_error() {
        exc::init();
        let parser = new_parser(None);
        assert!(!exc::pending());
        let result = parse(parser, "<a><b></a>");
        assert!(exc::pending());
        let error = exc::pending_value();
        let code = value::instance_attr(error, "code")
            .and_then(value::as_integer)
            .expect("error code attribute");
        let line = value::instance_attr(error, "lineno")
            .and_then(value::as_integer)
            .expect("error line attribute");
        assert!(line >= 1);
        let message = call("ErrorString", &[int_value(code)]);
        assert_eq!(value::as_str(message), Some("mismatched tag"));
        assert_eq!(result, NONE);
        exc::clear();
    }

    #[test]
    fn parse_file_reads_from_a_binary_stream() {
        exc::init();
        clear_events();
        let parser = new_parser(None);
        attach_handler(
            parser,
            "CharacterDataHandler",
            recorder("_pyexpat_test_char"),
        );
        let payload = value::bytes_new(b"<doc>x</doc>".to_vec());
        let stream = crate::modules::call_native("io", "BytesIO", 1, [payload].as_ptr());
        assert!(!exc::pending());
        let result = call("_pyexpat_parse_file", &[parser, stream]);
        assert_eq!(value::as_integer(result), Some(1));
        assert!(!exc::pending());
        assert!(events().iter().any(|(name, args)| {
            name == "_pyexpat_test_char" && args.first().map(String::as_str) == Some("s:x")
        }));
    }

    #[test]
    fn errors_object_maps_messages_to_codes() {
        exc::init();
        let errors = constant("errors");
        let codes = methods::getattr(errors, b"codes".as_ptr(), 5);
        assert!(!exc::pending());
        let tag_mismatch = constant("XML_ERROR_TAG_MISMATCH");
        let expected = value::as_str(tag_mismatch).expect("message string");
        let found = value::as_dict(codes)
            .expect("codes dict")
            .iter()
            .find(|(key, _)| value::as_str(*key) == Some(expected))
            .map(|(_, code)| *code);
        assert_eq!(found.and_then(value::as_integer), Some(7));
    }
}

#[cfg(test)]
thread_local! {
    static TEST_EVENTS: std::cell::RefCell<Vec<(String, Vec<String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_test_handler(function: i64, args: &Args) -> bool {
    let Some(func) = value::as_function(function) else {
        return false;
    };
    let value::FunctionBody::Native { module, name } = &func.body else {
        return false;
    };
    if *module != MODULE || !name.starts_with("_pyexpat_test_") {
        return false;
    }
    let described: Vec<String> = args.values.iter().map(|item| describe(*item)).collect();
    TEST_EVENTS.with(|slot| slot.borrow_mut().push(((*name).to_string(), described)));
    true
}

#[cfg(test)]
fn describe(value_arg: i64) -> String {
    if let Some(text) = value::as_str(value_arg) {
        return format!("s:{text}");
    }
    if let Some(bytes) = value::as_bytes(value_arg) {
        return format!("b:{}", String::from_utf8_lossy(bytes));
    }
    if let Some(pairs) = value::as_dict(value_arg) {
        let entries: Vec<String> = pairs
            .iter()
            .map(|(key, item)| format!("{}={}", describe(*key), describe(*item)))
            .collect();
        return format!("d{{{}}}", entries.join(","));
    }
    if let Some(number) = value::as_integer(value_arg) {
        return format!("i:{number}");
    }
    if value_arg == NONE {
        return "None".to_string();
    }
    format!("?:{}", value::type_name(value_arg))
}
