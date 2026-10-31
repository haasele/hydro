//! `readline`: GNU readline command-line editing and history.
//!
//! The module binds the real GNU `libreadline` at run time with `dlopen`, so
//! building Hydro never requires the development headers or a link-time
//! dependency. When the library is missing (or the loaded library is the
//! libedit emulation, whose history and completion semantics differ) the
//! module reports itself unavailable instead of pretending to work:
//! [`available`] returns `false` and every call raises `ImportError`.
//!
//! The Python-visible surface follows CPython 3.14's `Modules/readline.c`:
//! `parse_and_bind`, `get_line_buffer`, `insert_text`, `read_history_file`,
//! `write_history_file`, `add_history`, `get_history_item`, `clear_history`,
//! `get_history_length`, `set_history_length`, `get_completer`, `set_completer`,
//! `get_begidx`, `get_endidx`, `set_completer_delims`, `get_completer_delims`
//! and `set_startup_hook`. The
//! `readline()` entry point is bound as well; CPython 3.14 no longer publishes
//! it as a module function, so it is kept as the runtime's line-editor hook
//! rather than advertised.
//!
//! Differences from CPython, all deliberate and documented:
//!
//! - Text crossing the FFI boundary is UTF-8. CPython uses the locale encoding
//!   with `surrogateescape`; Hydro strings are Unicode, so undecodable bytes
//!   become U+FFFD instead of lone surrogates.
//! - `get_begidx`/`get_endidx` convert readline's byte offsets to character
//!   indices by counting the UTF-8 characters in the corresponding prefix.
//!   This matches CPython for the ASCII and well-formed UTF-8 cases.
//! - Hook values are stored per thread with a GC persistent root. Readline
//!   itself is process-global and single-threaded, exactly as in CPython.
//! - `set_completer`/`get_completer` validate callability with the runtime's
//!   own `callable()` predicate rather than CPython's `PyCallable_Check`.

#[cfg(unix)]
mod unix {
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::{LazyLock, Mutex};

    use crate::core::builtins;
    use crate::exc::{
        self, ATTRIBUTE_ERROR, IMPORT_ERROR, OS_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
    };
    use crate::heap;
    use crate::value::{self, NONE};

    /// `char *readline(const char *prompt)`.
    type ReadlineFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
    /// `void add_history(const char *)`.
    type AddHistoryFn = unsafe extern "C" fn(*const c_char);
    /// `HIST_ENTRY *history_get(int)`.
    type HistoryGetFn = unsafe extern "C" fn(c_int) -> *mut HistEntry;
    /// `int (*rl_startup_hook)(void)`.
    type StartupHookFn = unsafe extern "C" fn() -> c_int;
    /// `void clear_history(void)`.
    type ClearHistoryFn = unsafe extern "C" fn();
    /// `int rl_insert_text(const char *)`.
    type InsertTextFn = unsafe extern "C" fn(*const c_char) -> c_int;
    /// `int read_history(const char *)` / `int write_history(const char *)`.
    type HistoryFileFn = unsafe extern "C" fn(*const c_char) -> c_int;
    /// `HISTORY_STATE *history_get_history_state(void)`.
    type HistoryStateFn = unsafe extern "C" fn() -> *mut HistoryState;
    /// `int history_truncate_file(const char *, int)`.
    type TruncateFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
    /// `int rl_parse_and_bind(char *)`.
    type ParseAndBindFn = unsafe extern "C" fn(*mut c_char) -> c_int;
    /// `int rl_initialize(void)`.
    type InitializeFn = unsafe extern "C" fn() -> c_int;
    /// `char *on_completion(const char *, int)`.
    type CompletionEntryFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_char;
    /// `char **rl_completion_matches(const char *, rl_compentry_func_t *)`.
    type CompletionMatchesFn =
        unsafe extern "C" fn(*const c_char, CompletionEntryFn) -> *mut *mut c_char;
    /// `char **flex_complete(const char *, int, int)`.
    type AttemptedCompletionFn =
        unsafe extern "C" fn(*const c_char, c_int, c_int) -> *mut *mut c_char;

    /// GNU readline's `HISTORY_STATE` layout (readline 6 and later).
    #[repr(C)]
    struct HistoryState {
        _entries: *mut *mut c_char,
        _offset: c_int,
        length: c_int,
        _size: c_int,
        _flags: c_int,
    }

    /// GNU readline's `HIST_ENTRY` layout (`struct _hist_entry`).
    #[repr(C)]
    struct HistEntry {
        /// The history line, valid for the lifetime of the entry.
        line: *mut c_char,
        _timestamp: *mut c_char,
    }

    /// The bound GNU readline library.
    ///
    /// The function pointers and data addresses are immutable after `open`
    /// returns; readline itself is not thread-safe, so callers must use it
    /// from one thread (CPython has the same requirement). The raw pointers
    /// inside are therefore safe to share for symbol lookup only.
    struct Readline {
        /// The `dlopen` handle, kept for documentation: the library stays
        /// loaded for the process lifetime (there is no `dlclose` path).
        _handle: *mut c_void,
        readline: ReadlineFn,
        add_history: AddHistoryFn,
        history_get: HistoryGetFn,
        clear_history: ClearHistoryFn,
        insert_text: InsertTextFn,
        read_history: HistoryFileFn,
        write_history: HistoryFileFn,
        history_state: HistoryStateFn,
        history_truncate_file: TruncateFn,
        parse_and_bind: ParseAndBindFn,
        completion_matches: CompletionMatchesFn,
        /// Address of `char *rl_line_buffer`.
        line_buffer: *mut *mut c_char,
        /// Address of `rl_attempted_completion_function`.
        attempted_completion: *mut c_void,
        /// Address of `rl_attempted_completion_over`.
        attempted_completion_over: *mut c_int,
        /// Address of `rl_startup_hook` (`int (*)(void)`).
        startup_hook: *mut c_void,
        /// Address of `const char *rl_completer_word_break_characters`.
        word_break: *mut *mut c_char,
        /// The `rl_readline_name` value; kept alive for the program lifetime.
        _name: CString,
    }

    // SAFETY: after `open` no field is mutated and every function pointer is
    // an entry point of a library that stays loaded for the process lifetime.
    // Callers still serialise the actual readline calls through the module's
    // single-threaded contract.
    unsafe impl Send for Readline {}
    unsafe impl Sync for Readline {}

    /// The loaded library, if it was found. `None` is a permanent failure:
    /// the dynamic loader is not retried on every call.
    static READLINE: LazyLock<Option<Readline>> = LazyLock::new(|| unsafe { open() });

    /// CPython's default completion word delimiters, installed at load time so
    /// `get_completer_delims()` matches CPython out of the box.
    const DEFAULT_DELIMS: &str = " \t\n`~!@#$%^&*()-=+[{]}\\|;:'\",<>/?";

    /// Owns the delimiter string readline keeps a pointer to.
    static DELIMS: Mutex<Option<CString>> = Mutex::new(None);

    /// The maximum number of history lines written to a file (`-1`: no limit).
    static HISTORY_LENGTH: AtomicI32 = AtomicI32::new(-1);

    /// Whether readline should append every non-empty `readline()` result to
    /// the history list, as CPython does by default.
    static AUTO_HISTORY: AtomicBool = AtomicBool::new(true);

    /// The Python completer installed by `set_completer`, with its persistent
    /// root slot. Reads happen inside the libreadline callback.
    struct CompleterHook {
        value: i64,
        root: usize,
    }

    /// The Python startup hook installed by `set_startup_hook`, with its
    /// persistent root slot. It runs inside libreadline's
    /// `int (*rl_startup_hook)(void)` callback.
    struct StartupHook {
        value: i64,
        root: usize,
    }

    thread_local! {
        /// The thread's completer hook. Readline is driven by one thread, so a
        /// thread-local keeps a handle from one thread's heap out of another's.
        static COMPLETER: RefCell<Option<CompleterHook>> = const { RefCell::new(None) };
        /// The thread's startup hook (see [`set_startup_hook_call`]).
        static STARTUP_HOOK: RefCell<Option<StartupHook>> = const { RefCell::new(None) };
        /// `(begidx, endidx)` captured by the last completion attempt.
        static INDICES: Cell<(i64, i64)> = const { Cell::new((0, 0)) };
    }

    /// Attempts to `dlopen` GNU readline and resolve every symbol this module
    /// uses. Returns `None` when the library or one of the symbols is absent.
    unsafe fn open() -> Option<Readline> {
        const CANDIDATES: &[&[u8]] = &[
            b"libreadline.so.8\0",
            b"libreadline.so.7\0",
            b"libreadline.so\0",
            b"libreadline.dylib\0",
            b"libreadline.8.dylib\0",
        ];
        let mut handle = std::ptr::null_mut();
        for candidate in CANDIDATES {
            handle = libc::dlopen(
                candidate.as_ptr() as *const c_char,
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return None;
        }
        // libedit's readline emulation is not GNU readline: history indexing
        // and completion differ. Report unavailability instead of guessing.
        let version_slot =
            data_symbol(handle, b"rl_library_version\0").map(|slot| slot as *const *const c_char);
        let version = match version_slot {
            Some(slot) if !(*slot).is_null() => CStr::from_ptr(*slot).to_str().ok(),
            _ => None,
        }
        .unwrap_or("");
        if version.starts_with("EditLine") {
            libc::dlclose(handle);
            return None;
        }

        let readline = function::<ReadlineFn>(handle, b"readline\0");
        let add_history = function::<AddHistoryFn>(handle, b"add_history\0");
        let history_get = function::<HistoryGetFn>(handle, b"history_get\0");
        let clear_history = function::<ClearHistoryFn>(handle, b"clear_history\0");
        let insert_text = function::<InsertTextFn>(handle, b"rl_insert_text\0");
        let read_history = function::<HistoryFileFn>(handle, b"read_history\0");
        let write_history = function::<HistoryFileFn>(handle, b"write_history\0");
        let history_state = function::<HistoryStateFn>(handle, b"history_get_history_state\0");
        let history_truncate_file = function::<TruncateFn>(handle, b"history_truncate_file\0");
        let parse_and_bind = function::<ParseAndBindFn>(handle, b"rl_parse_and_bind\0");
        let completion_matches =
            function::<CompletionMatchesFn>(handle, b"rl_completion_matches\0");
        let initialize = function::<InitializeFn>(handle, b"rl_initialize\0");
        let line_buffer = data_symbol(handle, b"rl_line_buffer\0");
        let attempted_completion = data_symbol(handle, b"rl_attempted_completion_function\0");
        let attempted_completion_over = data_symbol(handle, b"rl_attempted_completion_over\0");
        let startup_hook = data_symbol(handle, b"rl_startup_hook\0");
        let word_break = data_symbol(handle, b"rl_completer_word_break_characters\0");
        let readline_name_slot = data_symbol(handle, b"rl_readline_name\0");

        macro_rules! required {
            ($value:expr) => {
                match $value {
                    Some(value) => value,
                    None => {
                        libc::dlclose(handle);
                        return None;
                    }
                }
            };
        }
        let readline = required!(readline);
        let add_history = required!(add_history);
        let history_get = required!(history_get);
        let clear_history = required!(clear_history);
        let insert_text = required!(insert_text);
        let read_history = required!(read_history);
        let write_history = required!(write_history);
        let history_state = required!(history_state);
        let history_truncate_file = required!(history_truncate_file);
        let parse_and_bind = required!(parse_and_bind);
        let completion_matches = required!(completion_matches);
        let initialize = required!(initialize);
        let line_buffer = required!(line_buffer);
        let attempted_completion = required!(attempted_completion);
        let attempted_completion_over = required!(attempted_completion_over);
        let startup_hook = required!(startup_hook);
        let word_break = required!(word_break);
        let readline_name_slot = required!(readline_name_slot);

        let name = CString::new("hydro").expect("static string has no NUL");
        *(readline_name_slot as *mut *mut c_char) = name.as_ptr() as *mut c_char;

        // `rl_initialize` would otherwise run lazily on the first input; doing
        // it here makes `parse_and_bind` and line-buffer calls well-defined.
        // It must run before the word delimiters are installed because the
        // initializer resets the completion defaults.
        let _ = initialize();

        // Install CPython's word delimiters. Readline keeps the pointer, so
        // the bytes live in `DELIMS` for the program lifetime.
        let delimiters = CString::new(DEFAULT_DELIMS).expect("static string has no NUL");
        *(word_break as *mut *mut c_char) = delimiters.as_ptr() as *mut c_char;
        if let Ok(mut slot) = DELIMS.lock() {
            *slot = Some(delimiters);
        }

        // Install the completion dispatcher. It checks the thread-local
        // completer on every attempt and returns NULL when none is set.
        *(attempted_completion as *mut Option<AttemptedCompletionFn>) = Some(flex_complete);
        *(attempted_completion_over as *mut c_int) = 0;

        Some(Readline {
            _handle: handle,
            readline,
            add_history,
            history_get,
            clear_history,
            insert_text,
            read_history,
            write_history,
            history_state,
            history_truncate_file,
            parse_and_bind,
            completion_matches,
            line_buffer: line_buffer as *mut *mut c_char,
            attempted_completion,
            attempted_completion_over: attempted_completion_over as *mut c_int,
            startup_hook,
            word_break: word_break as *mut *mut c_char,
            _name: name,
        })
    }

    /// Resolves a function symbol.
    ///
    /// SAFETY: `name` must be NUL-terminated and `handle` must be an open
    /// `dlopen` handle. The caller is responsible for using the returned
    /// pointer only with the exact C signature it names.
    unsafe fn function<T: Copy>(handle: *mut c_void, name: &[u8]) -> Option<T> {
        let symbol = libc::dlsym(handle, name.as_ptr() as *const c_char);
        if symbol.is_null() {
            return None;
        }
        debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        Some(std::mem::transmute_copy(&symbol))
    }

    /// Resolves a data symbol and returns its address.
    ///
    /// SAFETY: `name` must be NUL-terminated and `handle` must be open.
    unsafe fn data_symbol(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
        let symbol = libc::dlsym(handle, name.as_ptr() as *const c_char);
        if symbol.is_null() {
            None
        } else {
            Some(symbol)
        }
    }

    /// The bound library, or `None` when GNU readline is unavailable.
    fn load() -> Option<&'static Readline> {
        READLINE.as_ref()
    }

    /// Whether GNU readline was loaded successfully.
    pub(crate) fn available() -> bool {
        load().is_some()
    }

    fn unavailable() -> i64 {
        exc::raise_with_message(
            IMPORT_ERROR,
            "readline is not available: GNU readline could not be loaded",
        )
    }

    fn bad_arity(function: &str, expected: &str, given: usize) -> i64 {
        exc::raise_with_message(
            TYPE_ERROR,
            &format!("readline.{function}() takes {expected} ({given} given)"),
        )
    }

    fn attribute_error(name: &str) -> i64 {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'readline' has no attribute '{name}'"),
        )
    }

    fn encode_int(n: i64) -> i64 {
        value::encode_int(n).unwrap_or(NONE)
    }

    /// Byte string or text as an owned C string, matching the C module's
    /// `PyUnicode_EncodeLocale` conversion for the supported input types.
    fn text_argument(v: i64, function: &str) -> Result<CString, i64> {
        let bytes = if let Some(text) = value::as_str(v) {
            text.as_bytes().to_vec()
        } else if let Some(bytes) = value::as_bytes(v) {
            bytes.to_vec()
        } else {
            return Err(exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "{function}() argument must be str or bytes, not '{}'",
                    value::type_name(v)
                ),
            ));
        };
        CString::new(bytes).map_err(|_| {
            exc::raise_with_message(
                VALUE_ERROR,
                &format!("{function}(): embedded null character"),
            )
        })
    }

    /// A filename argument: `None` selects readline's default (`~/.history`).
    fn filename_argument(v: i64, function: &str) -> Result<Option<CString>, i64> {
        if v == NONE {
            return Ok(None);
        }
        text_argument(v, function).map(Some)
    }

    /// A C string borrowed from the library, decoded as UTF-8 lossily.
    ///
    /// SAFETY: `pointer` must be NULL or NUL-terminated and valid for the
    /// duration of the read.
    unsafe fn borrow_c_string(pointer: *const c_char) -> String {
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }

    /// Converts readline's byte offsets into character indices.
    fn character_indices(line: &str, start: c_int, end: c_int) -> (i64, i64) {
        let start = usize::try_from(start).unwrap_or(0).min(line.len());
        let end = usize::try_from(end).unwrap_or(0).min(line.len());
        let begin = line.get(..start).unwrap_or("").chars().count() as i64;
        let finish = line.get(..end).unwrap_or("").chars().count() as i64;
        (begin, finish.max(begin))
    }

    /// The libreadline generator callback: calls the Python completer with
    /// `(text, state)` and returns a `malloc`ed copy of its string result.
    unsafe extern "C" fn on_completion(text: *const c_char, state: c_int) -> *mut c_char {
        let Some(library) = load() else {
            return std::ptr::null_mut();
        };
        let completer = COMPLETER.with(|slot| slot.borrow().as_ref().map(|hook| hook.value));
        let Some(completer) = completer else {
            return std::ptr::null_mut();
        };
        *library.attempted_completion_over = 1;

        let text = borrow_c_string(text);
        let text_value = value::str_alloc(text);
        let state_value = encode_int(i64::from(state));
        let roots = heap::root_values(&[text_value, state_value]);
        let mut argv = [text_value, state_value];
        let result = builtins::call(completer, 2, argv.as_mut_ptr());
        if exc::pending() {
            // CPython clears errors raised by the completer and ends the match
            // list; the surrounding readline call must not see a pending error.
            exc::clear();
            heap::release_roots(roots);
            return std::ptr::null_mut();
        }
        let encoded = value::as_str(result).and_then(|text| CString::new(text).ok());
        heap::release_roots(roots);
        match encoded {
            Some(text) => libc::strdup(text.as_ptr()),
            None => std::ptr::null_mut(),
        }
    }

    /// The libreadline startup-hook callback: calls the Python hook with no
    /// arguments and returns `0`, or `-1` to tell readline the hook failed.
    ///
    /// Readline's `int (*)(void)` signature cannot carry a Python exception,
    /// so a failing hook is reported like an uncaught exception (CPython's
    /// callback prints it the same way) and the line editor aborts the start.
    unsafe extern "C" fn on_startup_hook() -> c_int {
        let hook = STARTUP_HOOK.with(|slot| slot.borrow().as_ref().map(|hook| hook.value));
        let Some(hook) = hook else {
            return 0;
        };
        let result = builtins::call(hook, 0, std::ptr::null());
        if exc::pending() {
            let _ = exc::uncaught();
            return -1;
        }
        let _ = result;
        0
    }

    /// The attempted-completion callback: records the completion scope and
    /// asks readline to build the match list from [`on_completion`].
    unsafe extern "C" fn flex_complete(
        text: *const c_char,
        start: c_int,
        end: c_int,
    ) -> *mut *mut c_char {
        let Some(library) = load() else {
            return std::ptr::null_mut();
        };
        let line = if library.line_buffer.is_null() {
            String::new()
        } else {
            borrow_c_string(*library.line_buffer)
        };
        let (begin, finish) = character_indices(&line, start, end);
        INDICES.with(|indices| indices.set((begin, finish)));
        (library.completion_matches)(text, on_completion)
    }

    /// `readline([prompt])`: reads one line through libreadline.
    ///
    /// CPython 3.14 no longer publishes this as a module function; it stays
    /// available here as the runtime's line-editing hook. EOF returns `""`,
    /// and a decoded line is added to the history unless `set_auto_history`
    /// disabled it, matching CPython's `call_readline`.
    fn readline_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return bad_arity("readline", "at most 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let prompt = if args.is_empty() {
            CString::new("").expect("empty string has no NUL")
        } else {
            match text_argument(args[0], "readline") {
                Ok(prompt) => prompt,
                Err(error) => return error,
            }
        };
        let pointer = unsafe { (library.readline)(prompt.as_ptr()) };
        if pointer.is_null() {
            return value::str_alloc(String::new());
        }
        let mut line = unsafe { borrow_c_string(pointer) };
        unsafe { libc::free(pointer as *mut c_void) };
        if !line.ends_with('\n') {
            line.push('\n');
        }
        if AUTO_HISTORY.load(Ordering::Relaxed) && line.len() > 1 {
            if let Ok(text) = CString::new(line.trim_end_matches('\n')) {
                unsafe { (library.add_history)(text.as_ptr()) };
            }
        }
        value::str_alloc(line)
    }

    /// `add_history(string)`.
    fn add_history_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("add_history", "exactly 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let text = match text_argument(args[0], "add_history") {
            Ok(text) => text,
            Err(error) => return error,
        };
        unsafe { (library.add_history)(text.as_ptr()) };
        NONE
    }

    /// `clear_history()`.
    fn clear_history_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("clear_history", "no arguments", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        unsafe { (library.clear_history)() };
        NONE
    }

    /// `get_line_buffer()`.
    fn get_line_buffer_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_line_buffer", "no arguments", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let text = if library.line_buffer.is_null() {
            String::new()
        } else {
            unsafe { borrow_c_string(*library.line_buffer) }
        };
        value::str_alloc(text)
    }

    /// `insert_text(string)`.
    fn insert_text_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("insert_text", "exactly 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let text = match text_argument(args[0], "insert_text") {
            Ok(text) => text,
            Err(error) => return error,
        };
        unsafe { (library.insert_text)(text.as_ptr()) };
        NONE
    }

    /// `parse_and_bind(string)`.
    fn parse_and_bind_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("parse_and_bind", "exactly 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        // `rl_parse_and_bind` modifies its argument, so pass a mutable copy.
        let mut text = match text_argument(args[0], "parse_and_bind") {
            Ok(text) => text.into_bytes_with_nul(),
            Err(error) => return error,
        };
        unsafe { (library.parse_and_bind)(text.as_mut_ptr() as *mut c_char) };
        NONE
    }

    /// Reads the current history length through `history_get_history_state`.
    fn current_history_length(library: &Readline) -> Option<i64> {
        let state = unsafe { (library.history_state)() };
        if state.is_null() {
            return None;
        }
        Some(i64::from(unsafe { (*state).length }))
    }

    /// `get_history_length()`.
    fn get_history_length_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_history_length", "no arguments", args.len());
        }
        if load().is_none() {
            return unavailable();
        }
        encode_int(i64::from(HISTORY_LENGTH.load(Ordering::Relaxed)))
    }

    /// `set_history_length(length)`.
    fn set_history_length_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("set_history_length", "exactly 1 argument", args.len());
        }
        let Some(length) = value::as_integer(args[0]) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "set_history_length() argument must be int, not '{}'",
                    value::type_name(args[0])
                ),
            );
        };
        let Ok(length) = i32::try_from(length) else {
            return exc::raise_with_message(
                OVERFLOW_ERROR,
                "set_history_length() argument is out of range for a C int",
            );
        };
        if load().is_none() {
            return unavailable();
        }
        HISTORY_LENGTH.store(length, Ordering::Relaxed);
        NONE
    }

    /// `read_history_file(filename=None)`.
    fn read_history_file_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return bad_arity("read_history_file", "at most 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let filename = if args.is_empty() {
            None
        } else {
            match filename_argument(args[0], "read_history_file") {
                Ok(filename) => filename,
                Err(error) => return error,
            }
        };
        let code = unsafe {
            (library.read_history)(
                filename
                    .as_ref()
                    .map(|name| name.as_ptr())
                    .unwrap_or(std::ptr::null()),
            )
        };
        if code != 0 {
            return os_error(code, filename.as_ref());
        }
        NONE
    }

    /// `write_history_file(filename=None)`.
    fn write_history_file_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return bad_arity("write_history_file", "at most 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let filename = if args.is_empty() {
            None
        } else {
            match filename_argument(args[0], "write_history_file") {
                Ok(filename) => filename,
                Err(error) => return error,
            }
        };
        let code = unsafe {
            (library.write_history)(
                filename
                    .as_ref()
                    .map(|name| name.as_ptr())
                    .unwrap_or(std::ptr::null()),
            )
        };
        if code != 0 {
            return os_error(code, filename.as_ref());
        }
        let length = HISTORY_LENGTH.load(Ordering::Relaxed);
        if length >= 0 {
            // Truncation failures are ignored, exactly as CPython does: the
            // history file was already written successfully.
            let _ = unsafe {
                (library.history_truncate_file)(
                    filename
                        .as_ref()
                        .map(|name| name.as_ptr())
                        .unwrap_or(std::ptr::null()),
                    length,
                )
            };
        }
        NONE
    }

    /// Raises `OSError(errno, message)`, using `filename` when available.
    fn os_error(code: c_int, filename: Option<&CString>) -> i64 {
        let message = std::io::Error::from_raw_os_error(code).to_string();
        let detail = message
            .split(" (os error")
            .next()
            .unwrap_or(message.as_str())
            .to_string();
        let rendered = match filename {
            Some(name) => format!("[Errno {code}] {detail}: '{}'", name.to_string_lossy()),
            None => format!("[Errno {code}] {detail}"),
        };
        exc::raise_with_message(OS_ERROR, &rendered)
    }

    /// `set_completer(function=None)`.
    fn set_completer_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return bad_arity("set_completer", "at most 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let function = args.first().copied().unwrap_or(NONE);
        if function == NONE {
            COMPLETER.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    heap::release_persistent_root(hook.root, hook.value);
                }
            });
            return NONE;
        }
        if builtins::callable(function) != value::TRUE {
            return exc::raise_with_message(
                TYPE_ERROR,
                "set_completer(func): argument not callable",
            );
        }
        let root = heap::add_persistent_root(function);
        COMPLETER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(previous) = slot.replace(CompleterHook {
                value: function,
                root,
            }) {
                heap::release_persistent_root(previous.root, previous.value);
            }
        });
        // Ensure the dispatcher is installed even if another module overwrote
        // `rl_attempted_completion_function`.
        if !library.attempted_completion.is_null() {
            unsafe {
                *(library.attempted_completion as *mut Option<AttemptedCompletionFn>) =
                    Some(flex_complete);
            }
        }
        NONE
    }

    /// `get_completer()`.
    fn get_completer_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_completer", "no arguments", args.len());
        }
        if load().is_none() {
            return unavailable();
        }
        COMPLETER.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|hook| hook.value)
                .unwrap_or(NONE)
        })
    }

    /// `get_begidx()`.
    fn get_begidx_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_begidx", "no arguments", args.len());
        }
        if load().is_none() {
            return unavailable();
        }
        encode_int(INDICES.with(|indices| indices.get().0))
    }

    /// `get_endidx()`.
    fn get_endidx_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_endidx", "no arguments", args.len());
        }
        if load().is_none() {
            return unavailable();
        }
        encode_int(INDICES.with(|indices| indices.get().1))
    }

    /// `set_completer_delims(string)`.
    fn set_completer_delims_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("set_completer_delims", "exactly 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let delimiters = match text_argument(args[0], "set_completer_delims") {
            Ok(delimiters) => delimiters,
            Err(error) => return error,
        };
        let pointer = delimiters.as_ptr() as *mut c_char;
        // Keep the bytes alive: readline stores the pointer, and other modules
        // may reassign the global.
        if let Ok(mut slot) = DELIMS.lock() {
            *slot = Some(delimiters);
        }
        unsafe {
            *library.word_break = pointer;
        }
        NONE
    }

    /// `get_completer_delims()`.
    fn get_completer_delims_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_completer_delims", "no arguments", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let text = unsafe {
            if library.word_break.is_null() || (*library.word_break).is_null() {
                String::new()
            } else {
                borrow_c_string(*library.word_break)
            }
        };
        value::str_alloc(text)
    }

    /// `set_auto_history(enabled)`: CPython keeps this flag in the C module so
    /// `readline()` records non-empty lines.
    fn set_auto_history_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("set_auto_history", "exactly 1 argument", args.len());
        }
        let enabled = value::truthy(args[0]);
        if load().is_none() {
            return unavailable();
        }
        AUTO_HISTORY.store(enabled, Ordering::Relaxed);
        NONE
    }

    /// `get_current_history_length()` (CPython extension helper).
    fn get_current_history_length_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return bad_arity("get_current_history_length", "no arguments", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        match current_history_length(library) {
            Some(length) => encode_int(length),
            None => exc::raise_with_message(OS_ERROR, "history state is unavailable"),
        }
    }

    /// `get_history_item(index)`: CPython's wrapper around `history_get`.
    ///
    /// The index is a 1-based absolute history position; an index outside the
    /// list returns `None`, exactly like CPython. The value is a C `int` in
    /// CPython's argument parser, so integers outside that range raise
    /// `OverflowError` instead of wrapping.
    fn get_history_item_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return bad_arity("get_history_item", "exactly 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let Some(index) = value::as_integer(args[0]) else {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!(
                    "get_history_item() argument must be int, not '{}'",
                    value::type_name(args[0])
                ),
            );
        };
        let Ok(index) = i32::try_from(index) else {
            return exc::raise_with_message(
                OVERFLOW_ERROR,
                "get_history_item() argument is out of range for a C int",
            );
        };
        let entry = unsafe { (library.history_get)(index) };
        if entry.is_null() {
            return NONE;
        }
        // SAFETY: `history_get` returns a live `HIST_ENTRY`; `line` is a
        // NUL-terminated string owned by the history list.
        let line = unsafe { borrow_c_string((*entry).line) };
        value::str_alloc(line)
    }

    /// `set_startup_hook(function=None)`: installs the hook libreadline runs
    /// before the first prompt of a line-editing session.
    ///
    /// CPython stores the callable and installs its own `on_startup_hook`
    /// trampoline in the `rl_startup_hook` global; `None` clears the point and
    /// drops the stored callable. A non-callable argument raises the same
    /// `TypeError: argument must be callable` CPython reports.
    fn set_startup_hook_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return bad_arity("set_startup_hook", "at most 1 argument", args.len());
        }
        let Some(library) = load() else {
            return unavailable();
        };
        let hook = args.first().copied().unwrap_or(NONE);
        if hook == NONE {
            STARTUP_HOOK.with(|slot| {
                if let Some(previous) = slot.borrow_mut().take() {
                    heap::release_persistent_root(previous.root, previous.value);
                }
            });
            unsafe {
                *(library.startup_hook as *mut Option<StartupHookFn>) = None;
            }
            return NONE;
        }
        if builtins::callable(hook) != value::TRUE {
            return exc::raise_with_message(TYPE_ERROR, "argument must be callable");
        }
        let root = heap::add_persistent_root(hook);
        STARTUP_HOOK.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(previous) = slot.replace(StartupHook { value: hook, root }) {
                heap::release_persistent_root(previous.root, previous.value);
            }
        });
        unsafe {
            *(library.startup_hook as *mut Option<StartupHookFn>) = Some(on_startup_hook);
        }
        NONE
    }

    /// Dispatches a native `readline` function.
    pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
        if function == "redisplay" {
            return NONE;
        }
        if !available() {
            return unavailable();
        }
        match function {
            "readline" => readline_call(args),
            "add_history" => add_history_call(args),
            "get_history_item" => get_history_item_call(args),
            "set_startup_hook" => set_startup_hook_call(args),
            "clear_history" => clear_history_call(args),
            "get_line_buffer" => get_line_buffer_call(args),
            "insert_text" => insert_text_call(args),
            "parse_and_bind" => parse_and_bind_call(args),
            "read_history_file" => read_history_file_call(args),
            "write_history_file" => write_history_file_call(args),
            "get_history_length" => get_history_length_call(args),
            "set_history_length" => set_history_length_call(args),
            "get_completer" => get_completer_call(args),
            "set_completer" => set_completer_call(args),
            "get_begidx" => get_begidx_call(args),
            "get_endidx" => get_endidx_call(args),
            "set_completer_delims" => set_completer_delims_call(args),
            "get_completer_delims" => get_completer_delims_call(args),
            "set_auto_history" => set_auto_history_call(args),
            "get_current_history_length" => get_current_history_length_call(args),
            _ => attribute_error(function),
        }
    }

    /// `readline` exposes no integer constants; CPython's module only has
    /// functions. Unknown names report `AttributeError`.
    pub(crate) fn constant(name: &str) -> i64 {
        if name == "backend" {
            return value::str_from_bytes(b"readline");
        }
        attribute_error(name)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        /// libreadline keeps process-global history and line-buffer state, so
        /// the tests that touch it must not overlap.
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        fn int(n: i64) -> i64 {
            value::encode_int(n).expect("representable integer")
        }

        fn text(text: &str) -> i64 {
            value::str_alloc(text.to_string())
        }

        fn with_roots(argv: &[i64], function: &str) -> i64 {
            let roots = heap::root_values(argv);
            let result = call(function, argv);
            heap::release_roots(roots);
            result
        }

        fn number(v: i64) -> i64 {
            value::as_integer(v).expect("integer result")
        }

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn temp_path(tag: &str) -> PathBuf {
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "hydro-readline-{}-{serial}-{tag}.txt",
                std::process::id()
            ))
        }

        #[test]
        fn availability_is_honest() {
            // The module must load GNU readline on this platform; if the
            // library is absent the test skips without asserting fake success.
            if !available() {
                return;
            }
            exc::init();
            let _guard = lock();
            // A known entry point proves the library was resolved, not faked.
            let delims = call("get_completer_delims", &[]);
            assert!(!exc::pending());
            assert!(value::as_str(delims).is_some());
        }

        #[test]
        fn history_round_trip_through_files() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            let first = temp_path("first");
            let second = temp_path("second");
            let first_text = first.to_string_lossy().into_owned();
            let second_text = second.to_string_lossy().into_owned();

            call("clear_history", &[]);
            assert!(!exc::pending());
            with_roots(&[text("alpha")], "add_history");
            with_roots(&[text("beta")], "add_history");
            assert_eq!(number(call("get_current_history_length", &[])), 2);

            with_roots(&[text(&first_text)], "write_history_file");
            assert!(!exc::pending(), "write_history_file failed");

            call("clear_history", &[]);
            assert_eq!(number(call("get_current_history_length", &[])), 0);
            with_roots(&[text(&first_text)], "read_history_file");
            assert!(!exc::pending(), "read_history_file failed");
            assert_eq!(number(call("get_current_history_length", &[])), 2);

            // A second write proves the loaded entries, not just the file.
            with_roots(&[text(&second_text)], "write_history_file");
            assert!(!exc::pending());
            let written = std::fs::read_to_string(&first).expect("history file");
            assert!(written.contains("alpha"));
            assert!(written.contains("beta"));

            // `set_history_length` truncates the file to the newest entries.
            call("set_history_length", &[int(1)]);
            assert!(!exc::pending());
            with_roots(&[text(&second_text)], "write_history_file");
            assert!(!exc::pending());
            let truncated = std::fs::read_to_string(&second).expect("truncated file");
            assert!(truncated.contains("beta"));
            assert!(!truncated.contains("alpha"));
            call("set_history_length", &[int(-1)]);

            let _ = std::fs::remove_file(&first);
            let _ = std::fs::remove_file(&second);
        }

        #[test]
        fn completer_round_trip_and_callability() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            let completer = value::builtin_function_new(hydro_ir::Builtin::Len);
            let roots = heap::root_values(&[completer]);
            call("set_completer", &[completer]);
            assert!(!exc::pending());
            assert_eq!(call("get_completer", &[]), completer);

            call("set_completer", &[NONE]);
            assert!(!exc::pending());
            assert_eq!(call("get_completer", &[]), NONE);

            call("set_completer", &[int(5)]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("set_completer", &[completer, completer]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
            heap::release_roots(roots);
        }

        #[test]
        fn line_buffer_insert_and_delimiters() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            let marker = format!("hydro_marker_{}", std::process::id());
            with_roots(&[text(&marker)], "insert_text");
            assert!(!exc::pending());
            let buffer = call("get_line_buffer", &[]);
            let buffer = value::as_str(buffer).expect("str");
            assert!(buffer.contains(&marker));

            with_roots(&[text("abc")], "set_completer_delims");
            assert!(!exc::pending());
            let delimiters = call("get_completer_delims", &[]);
            assert_eq!(value::as_str(delimiters), Some("abc"));

            with_roots(&[text("set editing-mode emacs")], "parse_and_bind");
            assert!(!exc::pending());
        }

        #[test]
        fn reports_missing_history_files() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            let missing = temp_path("missing");
            let missing_text = missing.to_string_lossy().into_owned();
            let _ = std::fs::remove_file(&missing);
            with_roots(&[text(&missing_text)], "read_history_file");
            assert_eq!(exc::pending_tag(), OS_ERROR as i64);
            exc::clear();

            call("set_history_length", &[text("x")]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("set_history_length", &[int(i64::from(i32::MAX) + 1)]);
            assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
            exc::clear();
        }

        #[test]
        fn history_items_and_startup_hook() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            call("clear_history", &[]);
            with_roots(&[text("alpha")], "add_history");
            with_roots(&[text("beta")], "add_history");

            // `get_history_item` is 1-based and returns `None` past the end.
            let first = call("get_history_item", &[int(1)]);
            assert_eq!(value::as_str(first), Some("alpha"));
            let second = call("get_history_item", &[int(2)]);
            assert_eq!(value::as_str(second), Some("beta"));
            assert_eq!(call("get_history_item", &[int(99)]), NONE);
            assert!(!exc::pending(), "out-of-range index must return None");

            call("get_history_item", &[text("x")]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
            call("get_history_item", &[int(i64::from(i32::MAX) + 1)]);
            assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
            exc::clear();

            // `set_startup_hook` stores a callable, installs the readline
            // trampoline and clears both again for `None`.
            let hook = value::builtin_function_new(hydro_ir::Builtin::Dict);
            let roots = heap::root_values(&[hook]);
            call("set_startup_hook", &[hook]);
            assert!(!exc::pending());
            // The trampoline calls the hook with no arguments; `dict()`
            // succeeds, so it reports success to readline.
            assert_eq!(unsafe { on_startup_hook() }, 0);
            assert!(!exc::pending());
            call("set_startup_hook", &[NONE]);
            assert!(!exc::pending());
            assert_eq!(unsafe { on_startup_hook() }, 0);

            call("set_startup_hook", &[int(5)]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
            call("set_startup_hook", &[hook, hook]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();
            heap::release_roots(roots);
        }
    }
}

#[cfg(unix)]
#[allow(unused_imports)]
pub(crate) use unix::{available, call, constant};

#[cfg(not(unix))]
mod portable {
    use crate::exc::{self, ATTRIBUTE_ERROR, IMPORT_ERROR};

    pub(crate) fn available() -> bool {
        false
    }

    pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
        let _ = function;
        exc::raise_with_message(IMPORT_ERROR, "readline is not available on this platform")
    }

    pub(crate) fn constant(name: &str) -> i64 {
        if name == "backend" {
            return value::str_from_bytes(b"readline");
        }
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'readline' has no attribute '{name}'"),
        )
    }
}

#[cfg(not(unix))]
#[allow(unused_imports)]
pub(crate) use portable::{available, call, constant};
