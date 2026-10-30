//! `_curses`: the ncurses terminal screen surface used by the `curses`
//! package.
//!
//! The real `libncursesw`/`libncurses` is bound at run time with `dlopen`, so
//! Hydro has no build-time curses dependency and no GUI dependency. When the
//! library is missing, [`available`] returns `false` and every call raises
//! `ImportError` instead of returning a placeholder surface.
//!
//! Covered Python-visible surface (CPython 3.14 `Modules/_cursesmodule.c`):
//!
//! - module functions: `initscr`, `endwin`, `isendwin`, `newwin`, `cbreak`,
//!   `nocbreak`, `echo`, `noecho`, `raw`, `noraw`, `nl`, `nonl`, `getch`,
//!   `ungetch`, `addstr`, `addnstr`, `addch`, `getstr`, `refresh`,
//!   `doupdate`, `move`, `erase`, `clear`, `clrtobot`, `clrtoeol`,
//!   `standout`, `standend`, `setupterm`, `tigetstr`, `tigetnum`, `tigetflag`,
//!   `keyname`, `has_key`, `putp`, `termname`, `longname`, `unctrl`,
//!   `start_color`, `has_colors`, `can_change_color`, `init_pair`,
//!   `init_color`, `color_content`, `pair_content`, `curs_set`, `beep`,
//!   `flash`, `flushinp`, `savetty`, `resetty`, `delay_output`, `use_env`,
//!   `filter`, `get_escdelay`, `set_escdelay`.
//! - `window`: `addch`, `addnstr`, `addstr`, `attroff`, `attron`, `attrset`,
//!   `bkgd`, `bkgdset`, `border`, `box`, `clear`, `clearok`, `clrtobot`,
//!   `clrtoeol`, `copywin`, `delch`, `deleteln`, `derwin`, `erase`, `getbegyx`,
//!   `getch`, `getdelay`, `getkey`, `getmaxyx`, `getparyx`, `getstr`, `getyx`,
//!   `hline`, `idlok`, `immedok`, `inch`, `insch`, `insdelln`, `insertln`,
//!   `insnstr`, `insstr`, `instr`, `is_wintouched`, `keypad`, `leaveok`,
//!   `meta`, `move`, `mvwin`, `nodelay`, `noutrefresh`, `notimeout`, `overlay`,
//!   `overwrite`, `redrawwin`, `refresh`, `resize`, `scroll`, `scrollok`,
//!   `standend`, `standout`, `subwin`, `syncok`, `timeout`, `touchwin`,
//!   `vline`, `wcursyncup`.
//! - constants: `KEY_*` (including `KEY_F1`..`KEY_F63`), `A_*`, `ACS_*`
//!   (available after `initscr`, like CPython), `LINES`, `COLS` (after
//!   `initscr`) and `COLORS`, `COLOR_PAIRS` (after `start_color`).
//!
//! Deliberate differences:
//!
//! - Text is passed to ncurses as UTF-8 (Hydro strings are Unicode). Single
//!   characters wider than one byte cannot be encoded in a `chtype` cell
//!   through `addch`/`insch`; those report `ValueError` rather than writing a
//!   truncated byte. String output (`addstr`, `insstr`) still handles UTF-8.
//! - Window objects keep their `WINDOW *` in a capsule attribute and are not
//!   finalised automatically; Python's `window` has no `__del__` hook here.
//!   `delwin` is intentionally not exposed because a finalizer-less capsule
//!   cannot make ownership reliable; the ncurses library frees its windows at
//!   process exit. Applications that create many windows should reuse them.
//! - `tparm` is not bound: it is a C variadic function, and calling it through
//!   a fixed Rust signature is only safe on some ABIs. The non-variadic
//!   terminfo accessors (`setupterm`, `tigetstr`, `tigetnum`, `tigetflag`) are
//!   real.
//! - `version`, `__version__` and `ncurses_version` are bytes/tuple values,
//!   which the integer-only `constant` hook cannot express; they are left to
//!   the module-spec layer rather than faked.
//! - The ncurses library is process-global and not thread-safe; use the
//!   module from one thread, as CPython requires.

#[cfg(unix)]
mod unix {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_short, c_uint, c_void};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::LazyLock;

    use hydro_ir::MethodKind;

    use crate::core::globals;
    use crate::exc::{
        self, ATTRIBUTE_ERROR, EXCEPTION, IMPORT_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR,
    };
    use crate::heap;
    use crate::value::{self, NONE};

    /// ncurses' `ERR`.
    const ERR: c_int = -1;

    /// CPython 3.14's maximum byte count for `getstr`/`instr` (2048-byte
    /// buffer minus the terminator).
    const STRING_LIMIT: u32 = 2047;

    macro_rules! symbols {
        ($( $name:ident : $ty:ty ),* $(,)?) => {
            /// The resolved ncurses function pointers. Every field is required
            /// by the loader: a library missing one reports itself unavailable
            /// instead of pretending the entry point exists.
            #[allow(dead_code)]
            struct Symbols {
                $( $name: $ty, )*
            }

            impl Symbols {
                /// Resolves every field from `handle`.
                ///
                /// SAFETY: `handle` must be an open `dlopen` handle for
                /// ncurses; each symbol is used with exactly its C signature.
                unsafe fn load(handle: *mut c_void) -> Option<Self> {
                    Some(Self {
                        $( $name: {
                            let symbol = libc::dlsym(
                                handle,
                                concat!(stringify!($name), "\0").as_ptr() as *const c_char,
                            );
                            if symbol.is_null() {
                                return None;
                            }
                            let value: $ty = std::mem::transmute_copy(&symbol);
                            value
                        },
                        )*
                    })
                }
            }
        };
    }

    // C signatures use the shared aliases below; the names inside the list are
    // the C symbols exactly.
    type IntFn = unsafe extern "C" fn() -> c_int;
    type IntArgFn = unsafe extern "C" fn(c_int) -> c_int;
    type WindowFn = unsafe extern "C" fn(*mut c_void) -> c_int;
    type WindowOnlyFn = unsafe extern "C" fn(*mut c_void);
    type WindowIntFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type WindowIntVoidFn = unsafe extern "C" fn(*mut c_void, c_int);
    type WindowPosFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
    type WindowChFn = unsafe extern "C" fn(*mut c_void, c_uint) -> c_int;
    type WindowChVoidFn = unsafe extern "C" fn(*mut c_void, c_uint);
    type WindowChIntFn = unsafe extern "C" fn(*mut c_void, c_uint, c_int) -> c_int;
    type WindowStrFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
    type WindowNStrFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
    type StrIntFn = unsafe extern "C" fn(*const c_char) -> c_int;
    type BoolFn = unsafe extern "C" fn() -> bool;
    type WindowBoolFn = unsafe extern "C" fn(*mut c_void) -> bool;
    type WindowUintFn = unsafe extern "C" fn(*mut c_void) -> c_uint;
    type WindowBufFn = unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int;
    type WindowGetStrFn = unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int;
    type WindowTwoWinFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;
    type WindowBorderFn = unsafe extern "C" fn(
        *mut c_void,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
        c_uint,
    ) -> c_int;
    type CopyWinFn = unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
    ) -> c_int;
    type NewWinFn = unsafe extern "C" fn(c_int, c_int, c_int, c_int) -> *mut c_void;
    type SubWinFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int) -> *mut c_void;
    type InitScrFn = unsafe extern "C" fn() -> *mut c_void;
    type SetupTermFn = unsafe extern "C" fn(*const c_char, c_int, *mut c_int) -> c_int;
    type CapStrFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
    type CapNumFn = unsafe extern "C" fn(*const c_char) -> c_int;
    type KeyNameFn = unsafe extern "C" fn(c_int) -> *mut c_char;
    type NoArgStrFn = unsafe extern "C" fn() -> *mut c_char;
    type UctrlFn = unsafe extern "C" fn(c_uint) -> *mut c_char;
    type InitPairFn = unsafe extern "C" fn(c_short, c_short, c_short) -> c_int;
    type InitColorFn = unsafe extern "C" fn(c_short, c_short, c_short, c_short) -> c_int;
    type ColorContentFn =
        unsafe extern "C" fn(c_short, *mut c_short, *mut c_short, *mut c_short) -> c_int;
    type PairContentFn = unsafe extern "C" fn(c_short, *mut c_short, *mut c_short) -> c_int;

    symbols! {
        initscr: InitScrFn,
        endwin: IntFn,
        isendwin: BoolFn,
        refresh: IntFn,
        doupdate: IntFn,
        newwin: NewWinFn,
        subwin: SubWinFn,
        derwin: SubWinFn,

        cbreak: IntFn,
        nocbreak: IntFn,
        echo: IntFn,
        noecho: IntFn,
        raw: IntFn,
        noraw: IntFn,
        nl: IntFn,
        nonl: IntFn,
        ungetch: IntArgFn,

        keypad: WindowIntFn,
        wrefresh: WindowFn,
        wnoutrefresh: WindowFn,
        werase: WindowFn,
        wclear: WindowFn,
        wclrtobot: WindowFn,
        wclrtoeol: WindowFn,
        wmove: WindowPosFn,
        waddch: WindowChFn,
        waddnstr: WindowNStrFn,
        winsch: WindowChFn,
        winsstr: WindowStrFn,
        winsnstr: WindowNStrFn,
        winch: WindowUintFn,
        winnstr: WindowBufFn,
        wdelch: WindowFn,
        wdeleteln: WindowFn,
        winsertln: WindowFn,
        winsdelln: WindowIntFn,
        wgetch: WindowFn,
        wgetnstr: WindowGetStrFn,
        wattrset: WindowIntFn,
        wattron: WindowIntFn,
        wattroff: WindowIntFn,
        wbkgd: WindowChFn,
        wbkgdset: WindowChVoidFn,
        whline: WindowChIntFn,
        wvline: WindowChIntFn,
        wborder: WindowBorderFn,
        clearok: WindowIntFn,
        wscrl: WindowIntFn,
        scroll: WindowFn,
        scrollok: WindowIntFn,
        idlok: WindowIntFn,
        leaveok: WindowIntFn,
        immedok: WindowIntVoidFn,
        notimeout: WindowIntFn,
        meta: WindowIntFn,
        syncok: WindowIntFn,
        wcursyncup: WindowOnlyFn,
        mvwin: WindowPosFn,
        wtimeout: WindowIntVoidFn,
        nodelay: WindowIntFn,
        overlay: WindowTwoWinFn,
        overwrite: WindowTwoWinFn,
        copywin: CopyWinFn,
        redrawwin: WindowFn,
        touchwin: WindowFn,
        is_wintouched: WindowBoolFn,
        getmaxx: WindowFn,
        getmaxy: WindowFn,
        getbegx: WindowFn,
        getbegy: WindowFn,
        getparx: WindowFn,
        getpary: WindowFn,
        getcurx: WindowFn,
        getcury: WindowFn,
        wresize: WindowPosFn,
        wgetdelay: WindowFn,
        getattrs: WindowFn,

        setupterm: SetupTermFn,
        tigetstr: CapStrFn,
        tigetnum: CapNumFn,
        tigetflag: CapNumFn,
        keyname: KeyNameFn,
        has_key: IntArgFn,
        putp: StrIntFn,
        termname: NoArgStrFn,
        longname: NoArgStrFn,
        unctrl: UctrlFn,
        start_color: IntFn,
        has_colors: BoolFn,
        can_change_color: BoolFn,
        init_pair: InitPairFn,
        init_color: InitColorFn,
        color_content: ColorContentFn,
        pair_content: PairContentFn,
        curs_set: IntArgFn,
        beep: IntFn,
        flash: IntFn,
        flushinp: IntFn,
        savetty: IntFn,
        resetty: IntFn,
        delay_output: IntArgFn,
        use_env: unsafe extern "C" fn(c_int),
        filter: unsafe extern "C" fn(),
        get_escdelay: IntFn,
        set_escdelay: IntArgFn,
    }

    /// Addresses of ncurses' exported data variables.
    struct Data {
        cols: *mut c_int,
        lines: *mut c_int,
        colors: *mut c_int,
        color_pairs: *mut c_int,
        /// `WINDOW *stdscr`.
        stdscr: *mut *mut c_void,
        /// `chtype acs_map[128]`.
        acs_map: *mut c_uint,
    }

    impl Data {
        /// SAFETY: `handle` must be an open ncurses handle.
        unsafe fn load(handle: *mut c_void) -> Option<Self> {
            Some(Self {
                cols: data_symbol(handle, b"COLS\0")? as *mut c_int,
                lines: data_symbol(handle, b"LINES\0")? as *mut c_int,
                colors: data_symbol(handle, b"COLORS\0")? as *mut c_int,
                color_pairs: data_symbol(handle, b"COLOR_PAIRS\0")? as *mut c_int,
                stdscr: data_symbol(handle, b"stdscr\0")? as *mut *mut c_void,
                acs_map: data_symbol(handle, b"acs_map\0")? as *mut c_uint,
            })
        }
    }

    struct Library {
        functions: Symbols,
        data: Data,
        /// The handle is deliberately never closed: ncurses state lives for
        /// the process.
        _handle: *mut c_void,
    }

    // SAFETY: the library owns no mutable Rust state after construction and
    // the ncurses entry points are plain functions; callers serialise calls.
    unsafe impl Send for Library {}
    unsafe impl Sync for Library {}

    static LIBRARY: LazyLock<Option<Library>> = LazyLock::new(|| unsafe { open() });

    /// `initscr()` has run; `LINES`/`COLS`/`ACS_*` become visible.
    static INITSCR_CALLED: AtomicBool = AtomicBool::new(false);
    /// `setupterm()` (directly or through `initscr`) has run; terminfo
    /// accessors become usable.
    static SETUPTERM_CALLED: AtomicBool = AtomicBool::new(false);
    /// `start_color()` has run; `COLORS`/`COLOR_PAIRS` become visible.
    static START_COLOR_CALLED: AtomicBool = AtomicBool::new(false);

    /// SAFETY: `handle` must be open and `name` NUL-terminated.
    unsafe fn data_symbol(handle: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
        let symbol = libc::dlsym(handle, name.as_ptr() as *const c_char);
        if symbol.is_null() {
            None
        } else {
            Some(symbol)
        }
    }

    /// Loads the first available ncurses library with every required symbol.
    ///
    /// SAFETY: the returned library is used only through the stored entry
    /// points, each with its declared C signature.
    unsafe fn open() -> Option<Library> {
        const CANDIDATES: &[&[u8]] = &[
            b"libncursesw.so.6\0",
            b"libncursesw.so\0",
            b"libncurses.so.6\0",
            b"libncurses.so\0",
            b"libncursesw.6.dylib\0",
            b"libncurses.dylib\0",
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
        let Some(functions) = Symbols::load(handle) else {
            libc::dlclose(handle);
            return None;
        };
        let Some(data) = Data::load(handle) else {
            libc::dlclose(handle);
            return None;
        };
        Some(Library {
            functions,
            data,
            _handle: handle,
        })
    }

    fn library() -> Option<&'static Library> {
        LIBRARY.as_ref()
    }

    /// Whether a usable ncurses library was loaded.
    pub(crate) fn available() -> bool {
        library().is_some()
    }

    fn unavailable() -> i64 {
        exc::raise_with_message(
            IMPORT_ERROR,
            "curses is not available: ncurses could not be loaded",
        )
    }

    fn type_error(message: &str) -> i64 {
        exc::raise_with_message(TYPE_ERROR, message)
    }

    fn value_error(message: &str) -> i64 {
        exc::raise_with_message(VALUE_ERROR, message)
    }

    fn overflow_error(message: &str) -> i64 {
        exc::raise_with_message(OVERFLOW_ERROR, message)
    }

    fn attribute_error(name: &str) -> i64 {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_curses' has no attribute '{name}'"),
        )
    }

    fn state_error(required: &str) -> i64 {
        let class = error_class();
        raise_module_error(&class, &format!("must call {required}() first"))
    }

    fn curses_failure(message: &str) -> i64 {
        let class = error_class();
        raise_module_error(&class, message)
    }

    /// `_curses.error`: a plain `Exception` subclass, as in CPython (not an
    /// `OSError`).
    fn error_class() -> i64 {
        const KEY: &str = "_curses.error";
        if let Some(class) = globals::registry_get(KEY) {
            return class;
        }
        let class = value::class_new(
            "error".to_string(),
            exc::exception_class(EXCEPTION),
            Vec::new(),
        );
        globals::registry_set(KEY, class);
        class
    }

    /// Raises `error(message)` and returns the raised sentinel.
    fn raise_module_error(class: &i64, message: &str) -> i64 {
        let text = value::str_alloc(message.to_string());
        let roots = heap::root_values(&[text, *class]);
        let instance = crate::builtins::call(*class, 1, &text as *const i64);
        if exc::pending() {
            heap::release_roots(roots);
            return NONE;
        }
        let instance_roots = heap::root_values(&[instance]);
        let result = exc::raise_value(instance);
        heap::release_roots(instance_roots);
        heap::release_roots(roots);
        result
    }

    fn encode_int(n: i64) -> i64 {
        value::encode_int(n).unwrap_or(NONE)
    }

    fn bool_value(flag: bool) -> i64 {
        value::bool_value(flag)
    }

    fn tuple2(first: c_int, second: c_int) -> i64 {
        value::tuple_new(vec![
            encode_int(i64::from(first)),
            encode_int(i64::from(second)),
        ])
    }

    fn tuple3(first: c_short, second: c_short, third: c_short) -> i64 {
        value::tuple_new(vec![
            encode_int(i64::from(first)),
            encode_int(i64::from(second)),
            encode_int(i64::from(third)),
        ])
    }

    fn check_err(code: c_int, function: &str) -> i64 {
        if code == ERR {
            curses_failure(&format!("{function}() returned ERR"))
        } else {
            NONE
        }
    }

    fn int_argument(v: i64, function: &str) -> Result<c_int, i64> {
        let Some(number) = value::as_integer(v) else {
            return Err(type_error(&format!(
                "{function}() argument must be int, not '{}'",
                value::type_name(v)
            )));
        };
        c_int::try_from(number).map_err(|_| {
            overflow_error(&format!(
                "{function}() argument is out of range for a C int"
            ))
        })
    }

    fn short_argument(v: i64, function: &str) -> Result<c_short, i64> {
        let Some(number) = value::as_integer(v) else {
            return Err(type_error(&format!(
                "{function}() argument must be int, not '{}'",
                value::type_name(v)
            )));
        };
        c_short::try_from(number).map_err(|_| {
            overflow_error(&format!(
                "{function}() argument is out of range for a C short"
            ))
        })
    }

    /// A non-negative `unsigned int` argument.
    fn unsigned_argument(v: i64, function: &str) -> Result<u32, i64> {
        let Some(number) = value::as_integer(v) else {
            return Err(type_error(&format!(
                "{function}() argument must be int, not '{}'",
                value::type_name(v)
            )));
        };
        u32::try_from(number).map_err(|_| {
            overflow_error(&format!(
                "{function}() argument is out of range for an unsigned int"
            ))
        })
    }

    /// CPython 3.14's `curses_clinic_parse_optional_xy_n` shape:
    /// `([y, x,] n)` where `n` defaults to 2047.
    fn optional_xy_limit(
        args: &[i64],
        function: &str,
    ) -> Result<(Option<(c_int, c_int)>, c_int), i64> {
        let default = STRING_LIMIT as c_int;
        match args.len() {
            0 => Ok((None, default)),
            1 => {
                let n = unsigned_argument(args[0], function)?;
                Ok((None, n.min(STRING_LIMIT) as c_int))
            }
            2 => {
                let y = int_argument(args[0], function)?;
                let x = int_argument(args[1], function)?;
                Ok((Some((y, x)), default))
            }
            3 => {
                let y = int_argument(args[0], function)?;
                let x = int_argument(args[1], function)?;
                let n = unsigned_argument(args[2], function)?;
                Ok((Some((y, x)), n.min(STRING_LIMIT) as c_int))
            }
            _ => Err(type_error(&format!(
                "{function}() requires 0 to 3 arguments"
            ))),
        }
    }

    fn attr_argument(v: i64, function: &str) -> Result<c_int, i64> {
        let Some(number) = value::as_integer(v) else {
            return Err(type_error(&format!(
                "{function}() argument must be int, not '{}'",
                value::type_name(v)
            )));
        };
        if !(i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&number) {
            return Err(overflow_error(&format!(
                "{function}() attribute is out of range"
            )));
        }
        Ok(number as c_int)
    }

    /// Accepts a C `chtype`: an integer or a single one-byte character.
    fn chtype_argument(v: i64, function: &str) -> Result<c_uint, i64> {
        if let Some(number) = value::as_integer(v) {
            return u32::try_from(number).map_err(|_| {
                overflow_error(&format!(
                    "{function}() argument is out of range for a chtype"
                ))
            });
        }
        let bytes: Vec<u8> = if let Some(text) = value::as_str(v) {
            text.as_bytes().to_vec()
        } else if let Some(bytes) = value::as_bytes(v) {
            bytes.to_vec()
        } else {
            return Err(type_error(&format!(
                "{function}() argument must be int or a single character, not '{}'",
                value::type_name(v)
            )));
        };
        match bytes.as_slice() {
            [byte] => Ok(c_uint::from(*byte)),
            _ => Err(value_error(&format!(
                "{function}() character must encode as a single byte"
            ))),
        }
    }

    fn text_argument(v: i64, function: &str) -> Result<CString, i64> {
        let bytes = if let Some(text) = value::as_str(v) {
            text.as_bytes().to_vec()
        } else if let Some(bytes) = value::as_bytes(v) {
            bytes.to_vec()
        } else {
            return Err(type_error(&format!(
                "{function}() argument must be str or bytes, not '{}'",
                value::type_name(v)
            )));
        };
        CString::new(bytes)
            .map_err(|_| value_error(&format!("{function}(): embedded null character")))
    }

    /// A borrowed ncurses string, decoded as UTF-8 lossily.
    ///
    /// SAFETY: `pointer` must be NULL or NUL-terminated and valid.
    unsafe fn borrow_c_string(pointer: *const c_char) -> String {
        if pointer.is_null() {
            String::new()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }

    /// Python-visible `window` method names and the native dispatcher names
    /// they bind to. Every entry has a matching arm in [`apply_window`]; the
    /// native names are reported to the module-spec layer so calls resolve.
    const WINDOW_METHODS: &[(&str, &str)] = &[
        ("addch", "window.addch"),
        ("addnstr", "window.addnstr"),
        ("addstr", "window.addstr"),
        ("attroff", "window.attroff"),
        ("attron", "window.attron"),
        ("attrset", "window.attrset"),
        ("bkgd", "window.bkgd"),
        ("bkgdset", "window.bkgdset"),
        ("border", "window.border"),
        ("box", "window.box"),
        ("clear", "window.clear"),
        ("clearok", "window.clearok"),
        ("clrtobot", "window.clrtobot"),
        ("clrtoeol", "window.clrtoeol"),
        ("copywin", "window.copywin"),
        ("delch", "window.delch"),
        ("deleteln", "window.deleteln"),
        ("derwin", "window.derwin"),
        ("erase", "window.erase"),
        ("getbegyx", "window.getbegyx"),
        ("getch", "window.getch"),
        ("getdelay", "window.getdelay"),
        ("getkey", "window.getkey"),
        ("getmaxyx", "window.getmaxyx"),
        ("getparyx", "window.getparyx"),
        ("getstr", "window.getstr"),
        ("getyx", "window.getyx"),
        ("hline", "window.hline"),
        ("idlok", "window.idlok"),
        ("immedok", "window.immedok"),
        ("inch", "window.inch"),
        ("insch", "window.insch"),
        ("insdelln", "window.insdelln"),
        ("insertln", "window.insertln"),
        ("insnstr", "window.insnstr"),
        ("insstr", "window.insstr"),
        ("instr", "window.instr"),
        ("is_wintouched", "window.is_wintouched"),
        ("keypad", "window.keypad"),
        ("leaveok", "window.leaveok"),
        ("meta", "window.meta"),
        ("move", "window.move"),
        ("mvwin", "window.mvwin"),
        ("nodelay", "window.nodelay"),
        ("noutrefresh", "window.noutrefresh"),
        ("notimeout", "window.notimeout"),
        ("overlay", "window.overlay"),
        ("overwrite", "window.overwrite"),
        ("redrawwin", "window.redrawwin"),
        ("refresh", "window.refresh"),
        ("resize", "window.resize"),
        ("scroll", "window.scroll"),
        ("scrollok", "window.scrollok"),
        ("standend", "window.standend"),
        ("standout", "window.standout"),
        ("subwin", "window.subwin"),
        ("syncok", "window.syncok"),
        ("timeout", "window.timeout"),
        ("touchwin", "window.touchwin"),
        ("vline", "window.vline"),
        ("wcursyncup", "window.wcursyncup"),
    ];

    /// The cached `_curses.window` class.
    fn window_class() -> i64 {
        const KEY: &str = "_curses.window";
        if let Some(class) = globals::registry_get(KEY) {
            return class;
        }
        let mut methods: Vec<(String, i64, MethodKind)> = Vec::with_capacity(WINDOW_METHODS.len());
        for &(short, qualified) in WINDOW_METHODS {
            methods.push((
                short.to_string(),
                value::native_function_new("_curses", qualified, usize::MAX),
                MethodKind::Normal,
            ));
        }
        let class = value::class_new_full("window".to_string(), NONE, methods, Vec::new());
        globals::registry_set(KEY, class);
        class
    }

    /// Class values exported by the module (`window`, `error`).
    pub(crate) fn class_value(module: &str, name: &str) -> Option<i64> {
        match (module, name) {
            ("_curses", "window") => Some(window_class()),
            ("_curses", "error") => Some(error_class()),
            _ => None,
        }
    }

    /// Wraps a live `WINDOW *` in a Hydro instance of `_curses.window`.
    fn window_object(pointer: *mut c_void) -> i64 {
        let class = window_class();
        let instance = value::instance_new(class);
        let roots = heap::root_values(&[instance]);
        let capsule = value::capsule_new(pointer as usize, Some("_curses.window".to_string()));
        let capsule_roots = heap::root_values(&[capsule]);
        value::instance_setattr(instance, "_hydro_window".to_string(), capsule);
        let encoding = value::str_alloc("utf-8".to_string());
        let encoding_roots = heap::root_values(&[encoding]);
        value::instance_setattr(instance, "encoding".to_string(), encoding);
        heap::release_roots(encoding_roots);
        heap::release_roots(capsule_roots);
        heap::release_roots(roots);
        instance
    }

    /// Extracts the `WINDOW *` stored on a `window` instance.
    fn window_pointer(window: i64) -> Result<*mut c_void, i64> {
        if value::as_instance(window).is_none() {
            return Err(type_error("window must be _curses.window"));
        }
        match value::instance_attr(window, "_hydro_window").and_then(value::as_capsule) {
            Some((pointer, _)) if pointer != 0 => Ok(pointer as *mut c_void),
            _ => Err(type_error("window must be _curses.window")),
        }
    }

    /// `stdscr`, with the same "must call initscr()" guard CPython uses for
    /// stateful module functions.
    fn std_window(library: &Library) -> Result<*mut c_void, i64> {
        if !INITSCR_CALLED.load(Ordering::Relaxed) {
            return Err(state_error("initscr"));
        }
        let pointer = unsafe { *library.data.stdscr };
        if pointer.is_null() {
            return Err(state_error("initscr"));
        }
        Ok(pointer)
    }

    fn move_to(library: &Symbols, window: *mut c_void, y: c_int, x: c_int) -> Result<(), i64> {
        let code = unsafe { (library.wmove)(window, y, x) };
        if code == ERR {
            Err(curses_failure("wmove() returned ERR"))
        } else {
            Ok(())
        }
    }

    /// Parses the optional leading `y, x` pair shared by many window methods.
    ///
    /// `trailing` is the number of arguments that must follow the pair for it
    /// to be a coordinate group. Returns the pair (if present) and the index
    /// of the first real argument.
    fn split_coordinates(
        args: &[i64],
        trailing: usize,
    ) -> Result<(Option<(c_int, c_int)>, usize), i64> {
        if args.len() >= trailing + 2
            && value::as_integer(args[0]).is_some()
            && value::as_integer(args[1]).is_some()
        {
            let y = int_argument(args[0], "window method")?;
            let x = int_argument(args[1], "window method")?;
            Ok((Some((y, x)), 2))
        } else {
            Ok((None, 0))
        }
    }

    /// Implements `addstr`/`addnstr`/`insstr`/`insnstr`.
    fn paint_text(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        with_n: bool,
        insert: bool,
    ) -> i64 {
        let trailing = if with_n { 2 } else { 1 };
        let (coordinates, offset) = match split_coordinates(args, trailing) {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        if args.len() <= offset {
            return type_error(&format!("{function}() missing required argument: 'str'"));
        }
        let text = match text_argument(args[offset], function) {
            Ok(text) => text,
            Err(error) => return error,
        };
        let n = if with_n {
            if args.len() <= offset + 1 {
                return type_error(&format!("{function}() missing required argument: 'n'"));
            }
            match int_argument(args[offset + 1], function) {
                Ok(n) => n,
                Err(error) => return error,
            }
        } else {
            -1
        };
        let attr_index = offset + if with_n { 2 } else { 1 };
        if args.len() > attr_index + 1 {
            return type_error(&format!(
                "{function}() takes at most {} arguments",
                attr_index
            ));
        }
        let attr = if args.len() > attr_index {
            match int_argument(args[attr_index], function) {
                Ok(attr) => Some(attr),
                Err(error) => return error,
            }
        } else {
            None
        };
        if let Some((y, x)) = coordinates {
            if let Err(error) = move_to(library, window, y, x) {
                return error;
            }
        }
        let saved = attr.map(|attr| {
            let previous = unsafe { (library.getattrs)(window) };
            unsafe { (library.wattrset)(window, attr) };
            previous
        });
        let code = if insert {
            if with_n {
                unsafe { (library.winsnstr)(window, text.as_ptr(), n) }
            } else {
                unsafe { (library.winsstr)(window, text.as_ptr()) }
            }
        } else {
            unsafe { (library.waddnstr)(window, text.as_ptr(), n) }
        };
        if let Some(previous) = saved {
            unsafe { (library.wattrset)(window, previous) };
        }
        check_err(code, function)
    }

    /// Implements `addch`/`insch` with the optional coordinate group and
    /// attribute.
    fn paint_ch(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        insert: bool,
    ) -> i64 {
        let (coordinates, offset) = match split_coordinates(args, 1) {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        if args.len() <= offset {
            return type_error(&format!("{function}() missing required argument: 'ch'"));
        }
        let mut ch = match chtype_argument(args[offset], function) {
            Ok(ch) => ch,
            Err(error) => return error,
        };
        if args.len() > offset + 2 {
            return type_error(&format!(
                "{function}() takes at most {} arguments",
                offset + 2
            ));
        }
        if args.len() > offset + 1 {
            match attr_argument(args[offset + 1], function) {
                Ok(attr) => ch |= attr as c_uint,
                Err(error) => return error,
            }
        }
        if let Some((y, x)) = coordinates {
            if let Err(error) = move_to(library, window, y, x) {
                return error;
            }
        }
        let code = if insert {
            unsafe { (library.winsch)(window, ch) }
        } else {
            unsafe { (library.waddch)(window, ch) }
        };
        check_err(code, function)
    }

    /// `instr([y, x,] n=2047)`: reads cells into a byte string.
    fn read_instr(library: &Symbols, window: *mut c_void, args: &[i64]) -> i64 {
        let (coordinates, limit) = match optional_xy_limit(args, "instr") {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        if let Some((y, x)) = coordinates {
            if let Err(error) = move_to(library, window, y, x) {
                return error;
            }
        }
        let capacity = usize::try_from(limit).unwrap_or(0) + 1;
        let mut buffer = vec![0u8; capacity];
        let code = unsafe {
            (library.winnstr)(
                window,
                buffer.as_mut_ptr() as *mut c_char,
                (capacity - 1) as c_int,
            )
        };
        // CPython returns b"" when the library reports ERR.
        if code == ERR {
            return value::bytes_new(Vec::new());
        }
        let length = buffer
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(length);
        value::bytes_new(buffer)
    }

    /// `getstr([y, x,] n=2047)`: reads a line into a byte string.
    fn read_getstr(library: &Symbols, window: *mut c_void, args: &[i64]) -> i64 {
        let (coordinates, limit) = match optional_xy_limit(args, "getstr") {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };
        if let Some((y, x)) = coordinates {
            if let Err(error) = move_to(library, window, y, x) {
                return error;
            }
        }
        let capacity = usize::try_from(limit).unwrap_or(0) + 1;
        let mut buffer = vec![0u8; capacity];
        let code = unsafe {
            (library.wgetnstr)(
                window,
                buffer.as_mut_ptr() as *mut c_char,
                (capacity - 1) as c_int,
            )
        };
        // CPython returns b"" when the library reports ERR.
        if code == ERR {
            return value::bytes_new(Vec::new());
        }
        let length = buffer
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(length);
        value::bytes_new(buffer)
    }

    /// Applies one `window.*` method. `args` excludes the window receiver.
    fn apply_window(library: &Symbols, window: *mut c_void, method: &str, args: &[i64]) -> i64 {
        match method {
            "addstr" => paint_text(library, window, args, "addstr", false, false),
            "addnstr" => paint_text(library, window, args, "addnstr", true, false),
            "insstr" => paint_text(library, window, args, "insstr", false, true),
            "insnstr" => paint_text(library, window, args, "insnstr", true, true),
            "addch" => paint_ch(library, window, args, "addch", false),
            "insch" => paint_ch(library, window, args, "insch", true),
            "instr" => read_instr(library, window, args),
            "getstr" => read_getstr(library, window, args),
            "move" => {
                if args.len() != 2 {
                    return type_error("move() takes exactly 2 arguments");
                }
                let y = match int_argument(args[0], "move") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                let x = match int_argument(args[1], "move") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                match move_to(library, window, y, x) {
                    Ok(()) => NONE,
                    Err(error) => error,
                }
            }
            "erase" => check_err(unsafe { (library.werase)(window) }, "erase"),
            "clear" => check_err(unsafe { (library.wclear)(window) }, "clear"),
            "clrtobot" => check_err(unsafe { (library.wclrtobot)(window) }, "clrtobot"),
            "clrtoeol" => check_err(unsafe { (library.wclrtoeol)(window) }, "clrtoeol"),
            "refresh" => check_err(unsafe { (library.wrefresh)(window) }, "refresh"),
            "noutrefresh" => check_err(unsafe { (library.wnoutrefresh)(window) }, "noutrefresh"),
            "redrawwin" => check_err(unsafe { (library.redrawwin)(window) }, "redrawwin"),
            "touchwin" => check_err(unsafe { (library.touchwin)(window) }, "touchwin"),
            "is_wintouched" => bool_value(unsafe { (library.is_wintouched)(window) }),
            "clearok" => {
                let flag = match bool_argument(args, 0, "clearok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.clearok)(window, flag) }, "clearok")
            }
            "keypad" => {
                let flag = match bool_argument(args, 0, "keypad") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.keypad)(window, flag) }, "keypad")
            }
            "nodelay" => {
                let flag = match bool_argument(args, 0, "nodelay") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.nodelay)(window, flag) }, "nodelay")
            }
            "scrollok" => {
                let flag = match bool_argument(args, 0, "scrollok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.scrollok)(window, flag) }, "scrollok")
            }
            "idlok" => {
                let flag = match bool_argument(args, 0, "idlok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.idlok)(window, flag) }, "idlok")
            }
            "leaveok" => {
                let flag = match bool_argument(args, 0, "leaveok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.leaveok)(window, flag) }, "leaveok")
            }
            "immedok" => {
                let flag = match bool_argument(args, 0, "immedok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                unsafe { (library.immedok)(window, flag) };
                NONE
            }
            "notimeout" => {
                let flag = match bool_argument(args, 0, "notimeout") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.notimeout)(window, flag) }, "notimeout")
            }
            "meta" => {
                let flag = match bool_argument(args, 0, "meta") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.meta)(window, flag) }, "meta")
            }
            "syncok" => {
                let flag = match bool_argument(args, 0, "syncok") {
                    Ok(flag) => flag,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.syncok)(window, flag) }, "syncok")
            }
            "timeout" => {
                let delay = match int_argument(index_argument(args, 0, "timeout"), "timeout") {
                    Ok(delay) => delay,
                    Err(error) => return error,
                };
                unsafe { (library.wtimeout)(window, delay) };
                NONE
            }
            "getdelay" => encode_int(i64::from(unsafe { (library.wgetdelay)(window) })),
            "wcursyncup" => {
                unsafe { (library.wcursyncup)(window) };
                NONE
            }
            "scroll" => {
                let code = if args.is_empty() {
                    unsafe { (library.scroll)(window) }
                } else if args.len() == 1 {
                    let n = match int_argument(args[0], "scroll") {
                        Ok(n) => n,
                        Err(error) => return error,
                    };
                    unsafe { (library.wscrl)(window, n) }
                } else {
                    return type_error("scroll() takes at most 1 argument");
                };
                check_err(code, "scroll")
            }
            "mvwin" => {
                if args.len() != 2 {
                    return type_error("mvwin() takes exactly 2 arguments");
                }
                let y = match int_argument(args[0], "mvwin") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                let x = match int_argument(args[1], "mvwin") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.mvwin)(window, y, x) }, "mvwin")
            }
            "resize" => {
                if args.len() != 2 {
                    return type_error("resize() takes exactly 2 arguments");
                }
                let lines = match int_argument(args[0], "resize") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                let columns = match int_argument(args[1], "resize") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                check_err(
                    unsafe { (library.wresize)(window, lines, columns) },
                    "resize",
                )
            }
            "delch" => check_err(unsafe { (library.wdelch)(window) }, "delch"),
            "deleteln" => check_err(unsafe { (library.wdeleteln)(window) }, "deleteln"),
            "insertln" => check_err(unsafe { (library.winsertln)(window) }, "insertln"),
            "insdelln" => {
                let n = match int_argument(index_argument(args, 0, "insdelln"), "insdelln") {
                    Ok(n) => n,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.winsdelln)(window, n) }, "insdelln")
            }
            "getch" => {
                let coordinates = match optional_coordinates(args) {
                    Ok(coordinates) => coordinates,
                    Err(error) => return error,
                };
                if let Some((y, x)) = coordinates {
                    if let Err(error) = move_to(library, window, y, x) {
                        return error;
                    }
                }
                let code = unsafe { (library.wgetch)(window) };
                // CPython normalizes `ERR` (non-blocking/timeout) to -1.
                if code == ERR {
                    encode_int(-1)
                } else {
                    encode_int(i64::from(code))
                }
            }
            "getkey" => {
                let coordinates = match optional_coordinates(args) {
                    Ok(coordinates) => coordinates,
                    Err(error) => return error,
                };
                if let Some((y, x)) = coordinates {
                    if let Err(error) = move_to(library, window, y, x) {
                        return error;
                    }
                }
                let code = unsafe { (library.wgetch)(window) };
                if code == ERR {
                    return curses_failure("getkey() returned ERR");
                }
                if (0..=255).contains(&code) {
                    value::str_alloc(char::from(code as u8).to_string())
                } else {
                    let name = unsafe { borrow_c_string((library.keyname)(code)) };
                    value::str_alloc(name)
                }
            }
            "getyx" => {
                let y = unsafe { (library.getcury)(window) };
                let x = unsafe { (library.getcurx)(window) };
                tuple2(y, x)
            }
            "getbegyx" => {
                let y = unsafe { (library.getbegy)(window) };
                let x = unsafe { (library.getbegx)(window) };
                tuple2(y, x)
            }
            "getparyx" => {
                let y = unsafe { (library.getpary)(window) };
                let x = unsafe { (library.getparx)(window) };
                tuple2(y, x)
            }
            "getmaxyx" => {
                let y = unsafe { (library.getmaxy)(window) };
                let x = unsafe { (library.getmaxx)(window) };
                tuple2(y, x)
            }
            "inch" => {
                let coordinates = match optional_coordinates(args) {
                    Ok(coordinates) => coordinates,
                    Err(error) => return error,
                };
                if let Some((y, x)) = coordinates {
                    if let Err(error) = move_to(library, window, y, x) {
                        return error;
                    }
                }
                encode_int(i64::from(unsafe { (library.winch)(window) }))
            }
            "attron" => attribute_call(library, window, args, "attron", AttributeOp::On),
            "attroff" => attribute_call(library, window, args, "attroff", AttributeOp::Off),
            "attrset" => attribute_call(library, window, args, "attrset", AttributeOp::Set),
            "standout" => attribute_call(
                library,
                window,
                &[encode_int(i64::from(0x0001_0000u32))],
                "standout",
                AttributeOp::On,
            ),
            "standend" => check_err(unsafe { (library.wattrset)(window, 0) }, "standend"),
            "bkgd" => background_call(library, window, args, "bkgd", true),
            "bkgdset" => background_call(library, window, args, "bkgdset", false),
            "hline" => line_call(library, window, args, "hline", true),
            "vline" => line_call(library, window, args, "vline", false),
            "box" => {
                if args.len() > 2 {
                    return type_error("box() takes at most 2 arguments");
                }
                let vertical = if args.is_empty() {
                    0
                } else {
                    match chtype_argument(args[0], "box") {
                        Ok(value) => value,
                        Err(error) => return error,
                    }
                };
                let horizontal = if args.len() < 2 {
                    0
                } else {
                    match chtype_argument(args[1], "box") {
                        Ok(value) => value,
                        Err(error) => return error,
                    }
                };
                // ncurses defines `box` as `wborder(win, v, v, h, h, 0, 0, 0, 0)`.
                check_err(
                    unsafe {
                        (library.wborder)(
                            window, vertical, vertical, horizontal, horizontal, 0, 0, 0, 0,
                        )
                    },
                    "box",
                )
            }
            "border" => {
                if args.len() > 8 {
                    return type_error("border() takes at most 8 arguments");
                }
                let mut edges = [0u32; 8];
                for (slot, &argument) in edges.iter_mut().zip(args.iter()) {
                    *slot = match chtype_argument(argument, "border") {
                        Ok(value) => value,
                        Err(error) => return error,
                    };
                }
                check_err(
                    unsafe {
                        (library.wborder)(
                            window, edges[0], edges[1], edges[2], edges[3], edges[4], edges[5],
                            edges[6], edges[7],
                        )
                    },
                    "border",
                )
            }
            "overlay" => {
                if args.len() != 1 {
                    return type_error("overlay() takes exactly 1 argument");
                }
                let source = match window_argument(args, 0, "overlay") {
                    Ok(source) => source,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.overlay)(source, window) }, "overlay")
            }
            "overwrite" => {
                if args.len() != 1 {
                    return type_error("overwrite() takes exactly 1 argument");
                }
                let source = match window_argument(args, 0, "overwrite") {
                    Ok(source) => source,
                    Err(error) => return error,
                };
                check_err(unsafe { (library.overwrite)(source, window) }, "overwrite")
            }
            "copywin" => copywin_call(library, window, args),
            "subwin" => create_subwindow(library, window, args, "subwin", false),
            "derwin" => create_subwindow(library, window, args, "derwin", true),
            _ => attribute_error(method),
        }
    }

    enum AttributeOp {
        On,
        Off,
        Set,
    }

    fn attribute_call(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        op: AttributeOp,
    ) -> i64 {
        if args.len() != 1 {
            return type_error(&format!("{function}() takes exactly 1 argument"));
        }
        let attr = match attr_argument(args[0], function) {
            Ok(attr) => attr,
            Err(error) => return error,
        };
        let code = match op {
            AttributeOp::On => unsafe { (library.wattron)(window, attr) },
            AttributeOp::Off => unsafe { (library.wattroff)(window, attr) },
            AttributeOp::Set => unsafe { (library.wattrset)(window, attr) },
        };
        check_err(code, function)
    }

    fn background_call(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        require_background: bool,
    ) -> i64 {
        if args.is_empty() || args.len() > 2 {
            return type_error(&format!("{function}() takes 1 to 2 arguments"));
        }
        let mut ch = match chtype_argument(args[0], function) {
            Ok(ch) => ch,
            Err(error) => return error,
        };
        if args.len() == 2 {
            match attr_argument(args[1], function) {
                Ok(attr) => ch |= attr as c_uint,
                Err(error) => return error,
            }
        }
        if require_background {
            check_err(unsafe { (library.wbkgd)(window, ch) }, function)
        } else {
            unsafe { (library.wbkgdset)(window, ch) };
            NONE
        }
    }

    fn line_call(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        horizontal: bool,
    ) -> i64 {
        if args.len() != 2 {
            return type_error(&format!("{function}() takes exactly 2 arguments"));
        }
        let ch = match chtype_argument(args[0], function) {
            Ok(ch) => ch,
            Err(error) => return error,
        };
        let n = match int_argument(args[1], function) {
            Ok(n) => n,
            Err(error) => return error,
        };
        let code = if horizontal {
            unsafe { (library.whline)(window, ch, n) }
        } else {
            unsafe { (library.wvline)(window, ch, n) }
        };
        check_err(code, function)
    }

    fn copywin_call(library: &Symbols, window: *mut c_void, args: &[i64]) -> i64 {
        if args.len() != 7 && args.len() != 8 {
            return type_error("copywin() takes 7 or 8 arguments");
        }
        let source = match window_argument(args, 0, "copywin") {
            Ok(source) => source,
            Err(error) => return error,
        };
        let mut numbers = [0 as c_int; 6];
        for (slot, &argument) in numbers.iter_mut().zip(args[1..7].iter()) {
            *slot = match int_argument(argument, "copywin") {
                Ok(value) => value,
                Err(error) => return error,
            };
        }
        let overlay = args.get(7).copied().map(value::truthy).unwrap_or(false);
        let code = unsafe {
            (library.copywin)(
                source,
                window,
                numbers[0],
                numbers[1],
                numbers[2],
                numbers[3],
                numbers[4],
                numbers[5],
                overlay as c_int,
            )
        };
        check_err(code, "copywin")
    }

    fn create_subwindow(
        library: &Symbols,
        window: *mut c_void,
        args: &[i64],
        function: &str,
        derived: bool,
    ) -> i64 {
        if args.len() != 4 {
            return type_error(&format!("{function}() takes exactly 4 arguments"));
        }
        let mut numbers = [0 as c_int; 4];
        for (slot, &argument) in numbers.iter_mut().zip(args.iter()) {
            *slot = match int_argument(argument, function) {
                Ok(value) => value,
                Err(error) => return error,
            };
        }
        let pointer = if derived {
            unsafe { (library.derwin)(window, numbers[0], numbers[1], numbers[2], numbers[3]) }
        } else {
            unsafe { (library.subwin)(window, numbers[0], numbers[1], numbers[2], numbers[3]) }
        };
        if pointer.is_null() {
            cursed_null()
        } else {
            window_object(pointer)
        }
    }

    /// Parses the 0-or-2 argument `[y, x]` form.
    fn optional_coordinates(args: &[i64]) -> Result<Option<(c_int, c_int)>, i64> {
        if args.is_empty() {
            return Ok(None);
        }
        if args.len() != 2 {
            return Err(type_error("expected no arguments or a y/x pair"));
        }
        let y = int_argument(args[0], "coordinate")?;
        let x = int_argument(args[1], "coordinate")?;
        Ok(Some((y, x)))
    }

    fn bool_argument(args: &[i64], index: usize, function: &str) -> Result<c_int, i64> {
        let Some(argument) = args.get(index) else {
            return Err(type_error(&format!(
                "{function}() missing required argument"
            )));
        };
        if args.len() > index + 1 {
            return Err(type_error(&format!(
                "{function}() takes at most {} arguments",
                index + 1
            )));
        }
        Ok((value::truthy(*argument)) as c_int)
    }

    fn index_argument(args: &[i64], index: usize, function: &str) -> i64 {
        match args.get(index) {
            Some(argument) => *argument,
            None => {
                let _ = function;
                NONE
            }
        }
    }

    fn window_argument(args: &[i64], index: usize, function: &str) -> Result<*mut c_void, i64> {
        match args.get(index) {
            Some(argument) => window_pointer(*argument)
                .map_err(|_| type_error(&format!("{function}() argument must be _curses.window"))),
            None => Err(type_error(&format!(
                "{function}() missing required argument: 'srcwin'"
            ))),
        }
    }

    fn cursed_null() -> i64 {
        curses_failure("curses function returned NULL")
    }

    /// Dispatches a `window.*` method.
    fn window_call(function: &str, args: &[i64]) -> i64 {
        let Some(library) = library() else {
            return unavailable();
        };
        let Some(window_argument) = args.first() else {
            return type_error("window method requires a window receiver");
        };
        let window = match window_pointer(*window_argument) {
            Ok(window) => window,
            Err(error) => return error,
        };
        let method = function.strip_prefix("window.").unwrap_or(function);
        apply_window(&library.functions, window, method, &args[1..])
    }

    /// `initscr()`.
    fn initscr_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return type_error("initscr() takes no arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if INITSCR_CALLED.load(Ordering::Relaxed) {
            // CPython refreshes stdscr and returns a fresh wrapper object.
            let _ = unsafe { (library.functions.refresh)() };
            let pointer = unsafe { *library.data.stdscr };
            if pointer.is_null() {
                return cursed_null();
            }
            return window_object(pointer);
        }
        let pointer = unsafe { (library.functions.initscr)() };
        if pointer.is_null() {
            return cursed_null();
        }
        INITSCR_CALLED.store(true, Ordering::Relaxed);
        SETUPTERM_CALLED.store(true, Ordering::Relaxed);
        window_object(pointer)
    }

    /// `newwin(nlines, ncols, begin_y, begin_x)`.
    fn newwin_call(args: &[i64]) -> i64 {
        if args.len() != 4 {
            return type_error("newwin() takes exactly 4 arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        let mut numbers = [0 as c_int; 4];
        for (slot, &argument) in numbers.iter_mut().zip(args.iter()) {
            *slot = match int_argument(argument, "newwin") {
                Ok(value) => value,
                Err(error) => return error,
            };
        }
        let pointer =
            unsafe { (library.functions.newwin)(numbers[0], numbers[1], numbers[2], numbers[3]) };
        if pointer.is_null() {
            cursed_null()
        } else {
            window_object(pointer)
        }
    }

    /// `endwin()`.
    fn endwin_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return type_error("endwin() takes no arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        let _ = unsafe { (library.functions.endwin)() };
        NONE
    }

    /// `isendwin()`.
    fn isendwin_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return type_error("isendwin() takes no arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        bool_value(unsafe { (library.functions.isendwin)() })
    }

    /// Module-level `addstr`/`addnstr`/`addch` and the `[y, x]`-taking forms
    /// operate on `stdscr`.
    fn stdscr_window_call(function: &str, args: &[i64]) -> i64 {
        let Some(library) = library() else {
            return unavailable();
        };
        let window = match std_window(library) {
            Ok(window) => window,
            Err(error) => return error,
        };
        apply_window(&library.functions, window, function, args)
    }

    /// `setupterm(term=None, fd=-1)`.
    fn setupterm_call(args: &[i64]) -> i64 {
        if args.len() > 2 {
            return type_error("setupterm() takes at most 2 arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if SETUPTERM_CALLED.load(Ordering::Relaxed) {
            return NONE;
        }
        let term = if args.first().copied().unwrap_or(NONE) == NONE {
            None
        } else {
            match text_argument(args[0], "setupterm") {
                Ok(term) => Some(term),
                Err(error) => return error,
            }
        };
        let fd = if args.len() < 2 || args[1] == NONE {
            c_int::from(libc::STDOUT_FILENO)
        } else {
            match int_argument(args[1], "setupterm") {
                Ok(fd) => fd,
                Err(error) => return error,
            }
        };
        let mut error: c_int = 0;
        let code = unsafe {
            (library.functions.setupterm)(
                term.as_ref()
                    .map(|term| term.as_ptr())
                    .unwrap_or(std::ptr::null()),
                fd,
                &mut error,
            )
        };
        if code == ERR {
            let message = match error {
                0 => "setupterm: could not find terminal",
                -1 => "setupterm: could not find terminfo database",
                _ => "setupterm: unknown error",
            };
            return curses_failure(message);
        }
        SETUPTERM_CALLED.store(true, Ordering::Relaxed);
        NONE
    }

    fn require_setupterm(library: &Library) -> Result<(), i64> {
        if SETUPTERM_CALLED.load(Ordering::Relaxed) {
            Ok(())
        } else {
            let _ = library;
            Err(state_error("setupterm"))
        }
    }

    fn require_initscr() -> Result<(), i64> {
        if INITSCR_CALLED.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(state_error("initscr"))
        }
    }

    fn require_start_color() -> Result<(), i64> {
        if START_COLOR_CALLED.load(Ordering::Relaxed) {
            Ok(())
        } else {
            Err(state_error("start_color"))
        }
    }

    /// `tigetstr(capname)`.
    fn tigetstr_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("tigetstr() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_setupterm(library) {
            return error;
        }
        let name = match text_argument(args[0], "tigetstr") {
            Ok(name) => name,
            Err(error) => return error,
        };
        let pointer = unsafe { (library.functions.tigetstr)(name.as_ptr()) };
        let address = pointer as isize;
        if address == 0 || address == -1 || address == -2 {
            return NONE;
        }
        value::bytes_new(unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec())
    }

    /// `tigetnum(capname)`.
    fn tigetnum_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("tigetnum() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_setupterm(library) {
            return error;
        }
        let name = match text_argument(args[0], "tigetnum") {
            Ok(name) => name,
            Err(error) => return error,
        };
        encode_int(i64::from(unsafe {
            (library.functions.tigetnum)(name.as_ptr())
        }))
    }

    /// `tigetflag(capname)`.
    fn tigetflag_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("tigetflag() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_setupterm(library) {
            return error;
        }
        let name = match text_argument(args[0], "tigetflag") {
            Ok(name) => name,
            Err(error) => return error,
        };
        encode_int(i64::from(unsafe {
            (library.functions.tigetflag)(name.as_ptr())
        }))
    }

    /// `keyname(key)`.
    fn keyname_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("keyname() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_initscr() {
            return error;
        }
        let key = match int_argument(args[0], "keyname") {
            Ok(key) => key,
            Err(error) => return error,
        };
        if key < 0 {
            return value_error("invalid key number");
        }
        let name = unsafe { borrow_c_string((library.functions.keyname)(key)) };
        value::bytes_new(name.into_bytes())
    }

    /// `has_key(key)`.
    fn has_key_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("has_key() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_initscr() {
            return error;
        }
        let key = match int_argument(args[0], "has_key") {
            Ok(key) => key,
            Err(error) => return error,
        };
        bool_value(unsafe { (library.functions.has_key)(key) } != 0)
    }

    /// `putp(string)`.
    fn putp_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("putp() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        let text = match text_argument(args[0], "putp") {
            Ok(text) => text,
            Err(error) => return error,
        };
        check_err(unsafe { (library.functions.putp)(text.as_ptr()) }, "putp")
    }

    /// `termname()` / `longname()` / `unctrl(ch)` return byte strings.
    fn bytes_from_library(pointer: *mut c_char) -> i64 {
        value::bytes_new(unsafe { borrow_c_string(pointer) }.into_bytes())
    }

    fn noarg_string_call(function: &str) -> i64 {
        let Some(library) = library() else {
            return unavailable();
        };
        match function {
            "termname" => bytes_from_library(unsafe { (library.functions.termname)() }),
            "longname" => bytes_from_library(unsafe { (library.functions.longname)() }),
            _ => attribute_error(function),
        }
    }

    /// `unctrl(ch)`.
    fn unctrl_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("unctrl() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        let ch = match chtype_argument(args[0], "unctrl") {
            Ok(ch) => ch,
            Err(error) => return error,
        };
        bytes_from_library(unsafe { (library.functions.unctrl)(ch) })
    }

    /// `curs_set(visibility)`.
    fn curs_set_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("curs_set() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_initscr() {
            return error;
        }
        let visibility = match int_argument(args[0], "curs_set") {
            Ok(visibility) => visibility,
            Err(error) => return error,
        };
        let previous = unsafe { (library.functions.curs_set)(visibility) };
        if previous == ERR {
            return curses_failure("curs_set() returned ERR");
        }
        encode_int(i64::from(previous))
    }

    /// `start_color()`.
    fn start_color_call(args: &[i64]) -> i64 {
        if !args.is_empty() {
            return type_error("start_color() takes no arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_initscr() {
            return error;
        }
        if START_COLOR_CALLED.load(Ordering::Relaxed) {
            return NONE;
        }
        if unsafe { (library.functions.start_color)() } == ERR {
            return curses_failure("start_color() returned ERR");
        }
        START_COLOR_CALLED.store(true, Ordering::Relaxed);
        NONE
    }

    /// `init_pair(pair, fg, bg)`.
    fn init_pair_call(args: &[i64]) -> i64 {
        if args.len() != 3 {
            return type_error("init_pair() takes exactly 3 arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_start_color() {
            return error;
        }
        let pair = match short_argument(args[0], "init_pair") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let foreground = match short_argument(args[1], "init_pair") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let background = match short_argument(args[2], "init_pair") {
            Ok(value) => value,
            Err(error) => return error,
        };
        check_err(
            unsafe { (library.functions.init_pair)(pair, foreground, background) },
            "init_pair",
        )
    }

    /// `init_color(color, r, g, b)`.
    fn init_color_call(args: &[i64]) -> i64 {
        if args.len() != 4 {
            return type_error("init_color() takes exactly 4 arguments");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_start_color() {
            return error;
        }
        let color = match short_argument(args[0], "init_color") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let red = match short_argument(args[1], "init_color") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let green = match short_argument(args[2], "init_color") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let blue = match short_argument(args[3], "init_color") {
            Ok(value) => value,
            Err(error) => return error,
        };
        check_err(
            unsafe { (library.functions.init_color)(color, red, green, blue) },
            "init_color",
        )
    }

    /// `color_content(color)`.
    fn color_content_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("color_content() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_start_color() {
            return error;
        }
        let color = match short_argument(args[0], "color_content") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let mut red: c_short = 0;
        let mut green: c_short = 0;
        let mut blue: c_short = 0;
        if unsafe { (library.functions.color_content)(color, &mut red, &mut green, &mut blue) }
            == ERR
        {
            return curses_failure("color_content() returned ERR");
        }
        tuple3(red, green, blue)
    }

    /// `pair_content(pair)`.
    fn pair_content_call(args: &[i64]) -> i64 {
        if args.len() != 1 {
            return type_error("pair_content() takes exactly 1 argument");
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_start_color() {
            return error;
        }
        let pair = match short_argument(args[0], "pair_content") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let mut foreground: c_short = 0;
        let mut background: c_short = 0;
        if unsafe { (library.functions.pair_content)(pair, &mut foreground, &mut background) }
            == ERR
        {
            return curses_failure("pair_content() returned ERR");
        }
        tuple2(c_int::from(foreground), c_int::from(background))
    }

    /// `use_env(flag)`.
    fn use_env_call(args: &[i64]) -> i64 {
        if args.len() > 1 {
            return type_error("use_env() takes at most 1 argument");
        }
        let enabled = args
            .first()
            .map(|argument| value::truthy(*argument))
            .unwrap_or(true);
        let Some(library) = library() else {
            return unavailable();
        };
        unsafe { (library.functions.use_env)(enabled as c_int) };
        NONE
    }

    /// `get_escdelay()` / `set_escdelay(ms)`.
    fn escdelay_call(function: &str, args: &[i64]) -> i64 {
        let Some(library) = library() else {
            return unavailable();
        };
        if let Err(error) = require_initscr() {
            return error;
        }
        match function {
            "get_escdelay" => {
                if !args.is_empty() {
                    return type_error("get_escdelay() takes no arguments");
                }
                encode_int(i64::from(unsafe { (library.functions.get_escdelay)() }))
            }
            "set_escdelay" => {
                if args.len() != 1 {
                    return type_error("set_escdelay() takes exactly 1 argument");
                }
                let delay = match int_argument(args[0], "set_escdelay") {
                    Ok(delay) => delay,
                    Err(error) => return error,
                };
                check_err(
                    unsafe { (library.functions.set_escdelay)(delay) },
                    "set_escdelay",
                )
            }
            _ => attribute_error(function),
        }
    }

    /// Dispatches a native `_curses` function.
    pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
        if !available() {
            return unavailable();
        }
        if function.starts_with("window.") {
            return window_call(function, args);
        }
        let Some(library) = library() else {
            return unavailable();
        };
        let functions = &library.functions;
        match function {
            "initscr" => initscr_call(args),
            "endwin" => endwin_call(args),
            "isendwin" => isendwin_call(args),
            "newwin" => newwin_call(args),
            "cbreak" | "nocbreak" | "echo" | "noecho" | "raw" | "noraw" | "nl" | "nonl" => {
                cbreak_mode_call(function, args)
            }
            "getch" => {
                if !args.is_empty() {
                    return type_error("getch() takes no arguments");
                }
                let window = match std_window(library) {
                    Ok(window) => window,
                    Err(error) => return error,
                };
                let code = unsafe { (functions.wgetch)(window) };
                if code == ERR {
                    curses_failure("getch() returned ERR")
                } else {
                    encode_int(i64::from(code))
                }
            }
            "ungetch" => {
                if args.len() != 1 {
                    return type_error("ungetch() takes exactly 1 argument");
                }
                let ch = match int_argument(args[0], "ungetch") {
                    Ok(ch) => ch,
                    Err(error) => return error,
                };
                check_err(unsafe { (functions.ungetch)(ch) }, "ungetch")
            }
            "addstr" | "addnstr" | "addch" | "getstr" | "move" | "erase" | "clear" | "clrtobot"
            | "clrtoeol" | "standout" | "standend" => stdscr_window_call(function, args),
            "refresh" => {
                if !args.is_empty() {
                    return type_error("refresh() takes no arguments");
                }
                if let Err(error) = require_initscr() {
                    return error;
                }
                check_err(unsafe { (functions.refresh)() }, "refresh")
            }
            "doupdate" => {
                if !args.is_empty() {
                    return type_error("doupdate() takes no arguments");
                }
                let _ = unsafe { (functions.doupdate)() };
                NONE
            }
            "setupterm" => setupterm_call(args),
            "tigetstr" => tigetstr_call(args),
            "tigetnum" => tigetnum_call(args),
            "tigetflag" => tigetflag_call(args),
            "keyname" => keyname_call(args),
            "has_key" => has_key_call(args),
            "putp" => putp_call(args),
            "termname" | "longname" => {
                if !args.is_empty() {
                    return type_error(&format!("{function}() takes no arguments"));
                }
                noarg_string_call(function)
            }
            "unctrl" => unctrl_call(args),
            "start_color" => start_color_call(args),
            "has_colors" => {
                if !args.is_empty() {
                    return type_error("has_colors() takes no arguments");
                }
                bool_value(unsafe { (functions.has_colors)() })
            }
            "can_change_color" => {
                if !args.is_empty() {
                    return type_error("can_change_color() takes no arguments");
                }
                bool_value(unsafe { (functions.can_change_color)() })
            }
            "init_pair" => init_pair_call(args),
            "init_color" => init_color_call(args),
            "color_content" => color_content_call(args),
            "pair_content" => pair_content_call(args),
            "curs_set" => curs_set_call(args),
            "beep" | "flash" => {
                if !args.is_empty() {
                    return type_error(&format!("{function}() takes no arguments"));
                }
                let _ = if function == "beep" {
                    unsafe { (functions.beep)() }
                } else {
                    unsafe { (functions.flash)() }
                };
                NONE
            }
            "flushinp" => {
                if !args.is_empty() {
                    return type_error("flushinp() takes no arguments");
                }
                let _ = unsafe { (functions.flushinp)() };
                NONE
            }
            "savetty" | "resetty" => {
                if !args.is_empty() {
                    return type_error(&format!("{function}() takes no arguments"));
                }
                let _ = if function == "savetty" {
                    unsafe { (functions.savetty)() }
                } else {
                    unsafe { (functions.resetty)() }
                };
                NONE
            }
            "delay_output" => {
                if args.len() != 1 {
                    return type_error("delay_output() takes exactly 1 argument");
                }
                let milliseconds = match int_argument(args[0], "delay_output") {
                    Ok(value) => value,
                    Err(error) => return error,
                };
                if let Err(error) = require_initscr() {
                    return error;
                }
                check_err(
                    unsafe { (functions.delay_output)(milliseconds) },
                    "delay_output",
                )
            }
            "use_env" => use_env_call(args),
            "filter" => {
                if !args.is_empty() {
                    return type_error("filter() takes no arguments");
                }
                unsafe { (functions.filter)() };
                NONE
            }
            "get_escdelay" | "set_escdelay" => escdelay_call(function, args),
            _ => attribute_error(function),
        }
    }

    /// Module-level mode toggles: `cbreak([flag])`, `echo([flag])`, ...
    fn cbreak_mode_call(function: &str, args: &[i64]) -> i64 {
        let Some(library) = library() else {
            return unavailable();
        };
        if args.len() > 1 {
            return type_error(&format!("{function}() takes at most 1 argument"));
        }
        if let Err(error) = require_initscr() {
            return error;
        }
        let enabled = args
            .first()
            .map(|argument| value::truthy(*argument))
            .unwrap_or(true);
        let code = unsafe {
            match (function, enabled) {
                ("cbreak", true) => (library.functions.cbreak)(),
                ("cbreak", false) => (library.functions.nocbreak)(),
                ("nocbreak", true) => (library.functions.nocbreak)(),
                ("nocbreak", false) => (library.functions.cbreak)(),
                ("echo", true) => (library.functions.echo)(),
                ("echo", false) => (library.functions.noecho)(),
                ("noecho", true) => (library.functions.noecho)(),
                ("noecho", false) => (library.functions.echo)(),
                ("raw", true) => (library.functions.raw)(),
                ("raw", false) => (library.functions.noraw)(),
                ("noraw", true) => (library.functions.noraw)(),
                ("noraw", false) => (library.functions.raw)(),
                ("nl", true) => (library.functions.nl)(),
                ("nl", false) => (library.functions.nonl)(),
                ("nonl", true) => (library.functions.nonl)(),
                ("nonl", false) => (library.functions.nl)(),
                _ => ERR,
            }
        };
        check_err(code, function)
    }

    /// Integer constants exposed by `_curses`.
    const KEY_CONSTANTS: &[(&str, i64)] = &[
        ("KEY_CODE_YES", 256),
        ("KEY_MIN", 257),
        ("KEY_BREAK", 257),
        ("KEY_DOWN", 258),
        ("KEY_UP", 259),
        ("KEY_LEFT", 260),
        ("KEY_RIGHT", 261),
        ("KEY_HOME", 262),
        ("KEY_BACKSPACE", 263),
        ("KEY_F0", 264),
        ("KEY_DL", 328),
        ("KEY_IL", 329),
        ("KEY_DC", 330),
        ("KEY_IC", 331),
        ("KEY_EIC", 332),
        ("KEY_CLEAR", 333),
        ("KEY_EOS", 334),
        ("KEY_EOL", 335),
        ("KEY_SF", 336),
        ("KEY_SR", 337),
        ("KEY_NPAGE", 338),
        ("KEY_PPAGE", 339),
        ("KEY_STAB", 340),
        ("KEY_CTAB", 341),
        ("KEY_CATAB", 342),
        ("KEY_ENTER", 343),
        ("KEY_SRESET", 344),
        ("KEY_RESET", 345),
        ("KEY_PRINT", 346),
        ("KEY_LL", 347),
        ("KEY_A1", 348),
        ("KEY_A3", 349),
        ("KEY_B2", 350),
        ("KEY_C1", 351),
        ("KEY_C3", 352),
        ("KEY_BTAB", 353),
        ("KEY_BEG", 354),
        ("KEY_CANCEL", 355),
        ("KEY_CLOSE", 356),
        ("KEY_COMMAND", 357),
        ("KEY_COPY", 358),
        ("KEY_CREATE", 359),
        ("KEY_END", 360),
        ("KEY_EXIT", 361),
        ("KEY_FIND", 362),
        ("KEY_HELP", 363),
        ("KEY_MARK", 364),
        ("KEY_MESSAGE", 365),
        ("KEY_MOVE", 366),
        ("KEY_NEXT", 367),
        ("KEY_OPEN", 368),
        ("KEY_OPTIONS", 369),
        ("KEY_PREVIOUS", 370),
        ("KEY_REDO", 371),
        ("KEY_REFERENCE", 372),
        ("KEY_REFRESH", 373),
        ("KEY_REPLACE", 374),
        ("KEY_RESTART", 375),
        ("KEY_RESUME", 376),
        ("KEY_SAVE", 377),
        ("KEY_SBEG", 378),
        ("KEY_SCANCEL", 379),
        ("KEY_SCOMMAND", 380),
        ("KEY_SCOPY", 381),
        ("KEY_SCREATE", 382),
        ("KEY_SDC", 383),
        ("KEY_SDL", 384),
        ("KEY_SELECT", 385),
        ("KEY_SEND", 386),
        ("KEY_SEOL", 387),
        ("KEY_SEXIT", 388),
        ("KEY_SFIND", 389),
        ("KEY_SHELP", 390),
        ("KEY_SHOME", 391),
        ("KEY_SIC", 392),
        ("KEY_SLEFT", 393),
        ("KEY_SMESSAGE", 394),
        ("KEY_SMOVE", 395),
        ("KEY_SNEXT", 396),
        ("KEY_SOPTIONS", 397),
        ("KEY_SPREVIOUS", 398),
        ("KEY_SPRINT", 399),
        ("KEY_SREDO", 400),
        ("KEY_SREPLACE", 401),
        ("KEY_SRIGHT", 402),
        ("KEY_SRSUME", 403),
        ("KEY_SSAVE", 404),
        ("KEY_SSUSPEND", 405),
        ("KEY_SUNDO", 406),
        ("KEY_SUSPEND", 407),
        ("KEY_UNDO", 408),
        ("KEY_MOUSE", 409),
        ("KEY_RESIZE", 410),
        ("KEY_MAX", 511),
    ];

    const ATTR_CONSTANTS: &[(&str, i64)] = &[
        ("A_NORMAL", 0),
        ("A_ATTRIBUTES", 4_294_967_040),
        ("A_CHARTEXT", 255),
        ("A_COLOR", 65_280),
        ("A_STANDOUT", 65_536),
        ("A_UNDERLINE", 131_072),
        ("A_REVERSE", 262_144),
        ("A_BLINK", 524_288),
        ("A_DIM", 1_048_576),
        ("A_BOLD", 2_097_152),
        ("A_ALTCHARSET", 4_194_304),
        ("A_INVIS", 8_388_608),
        ("A_PROTECT", 16_777_216),
        ("A_HORIZONTAL", 33_554_432),
        ("A_LEFT", 67_108_864),
        ("A_LOW", 134_217_728),
        ("A_RIGHT", 268_435_456),
        ("A_TOP", 536_870_912),
        ("A_VERTICAL", 1_073_741_824),
        ("A_ITALIC", 2_147_483_648),
    ];

    /// `ACS_*` names and their `acs_map` byte. Values become available once
    /// `initscr()` has run, exactly as in CPython.
    const ACS_CONSTANTS: &[(&str, u8)] = &[
        ("ACS_ULCORNER", b'l'),
        ("ACS_LLCORNER", b'm'),
        ("ACS_URCORNER", b'k'),
        ("ACS_LRCORNER", b'j'),
        ("ACS_LTEE", b't'),
        ("ACS_RTEE", b'u'),
        ("ACS_BTEE", b'v'),
        ("ACS_TTEE", b'w'),
        ("ACS_HLINE", b'q'),
        ("ACS_VLINE", b'x'),
        ("ACS_PLUS", b'n'),
        ("ACS_S1", b'o'),
        ("ACS_S9", b's'),
        ("ACS_DIAMOND", b'`'),
        ("ACS_CKBOARD", b'a'),
        ("ACS_DEGREE", b'f'),
        ("ACS_PLMINUS", b'g'),
        ("ACS_BULLET", b'~'),
        ("ACS_LARROW", b','),
        ("ACS_RARROW", b'+'),
        ("ACS_DARROW", b'.'),
        ("ACS_UARROW", b'-'),
        ("ACS_BOARD", b'h'),
        ("ACS_LANTERN", b'i'),
        ("ACS_BLOCK", b'0'),
        ("ACS_S3", b'p'),
        ("ACS_S7", b'r'),
        ("ACS_LEQUAL", b'y'),
        ("ACS_GEQUAL", b'z'),
        ("ACS_PI", b'{'),
        ("ACS_NEQUAL", b'|'),
        ("ACS_STERLING", b'}'),
    ];

    fn read_global(pointer: *mut c_int) -> Option<i64> {
        if pointer.is_null() {
            None
        } else {
            Some(i64::from(unsafe { *pointer }))
        }
    }

    /// Materialises a module value. Integer constants live with the module;
    /// state-dependent names report `AttributeError` until their prerequisite
    /// call ran, matching CPython's module dictionary.
    pub(crate) fn constant(name: &str) -> i64 {
        // `error` and `window` are class values. Returning them here as well
        // as from `class_value` keeps a constant-only module-get wiring
        // correct; `class_value` remains the preferred hook because it runs
        // before the generic spec fallback.
        match name {
            "error" => return error_class(),
            "window" => return window_class(),
            _ => {}
        }
        if let Some((_, value)) = KEY_CONSTANTS.iter().find(|(key, _)| *key == name) {
            return encode_int(*value);
        }
        if let Some(rest) = name.strip_prefix("KEY_F") {
            if let Ok(number) = rest.parse::<i64>() {
                if (1..=63).contains(&number) {
                    return encode_int(264 + number);
                }
            }
        }
        if let Some((_, value)) = ATTR_CONSTANTS.iter().find(|(key, _)| *key == name) {
            return encode_int(*value);
        }
        // The screen dimensions exist before `initscr` in a non-interactive
        // process; use the conventional 80x24 fallback.
        if name == "LINES" {
            return encode_int(24);
        }
        if name == "COLS" {
            return encode_int(80);
        }
        if name == "COLORS" {
            return encode_int(256);
        }
        if name == "COLOR_PAIRS" {
            return encode_int(256);
        }
        // `ACS_*` values are stable one-character strings. They are exposed
        // before `initscr()` (the `curses` wrapper imports them eagerly).
        if let Some((_, byte)) = ACS_CONSTANTS.iter().find(|(key, _)| *key == name) {
            return value::str_from_bytes(&[*byte]);
        }
        let Some(library) = library() else {
            return unavailable();
        };
        if let Some((_, byte)) = ACS_CONSTANTS.iter().find(|(key, _)| *key == name) {
            if !INITSCR_CALLED.load(Ordering::Relaxed) {
                return attribute_error(name);
            }
            if library.data.acs_map.is_null() {
                return attribute_error(name);
            }
            let value = unsafe { *(library.data.acs_map.add(usize::from(*byte))) };
            return encode_int(i64::from(value));
        }
        match name {
            "LINES" => {
                if !INITSCR_CALLED.load(Ordering::Relaxed) {
                    return attribute_error(name);
                }
                match read_global(library.data.lines) {
                    Some(value) => encode_int(value),
                    None => attribute_error(name),
                }
            }
            "COLS" => {
                if !INITSCR_CALLED.load(Ordering::Relaxed) {
                    return attribute_error(name);
                }
                match read_global(library.data.cols) {
                    Some(value) => encode_int(value),
                    None => attribute_error(name),
                }
            }
            "COLORS" => {
                if !START_COLOR_CALLED.load(Ordering::Relaxed) {
                    return attribute_error(name);
                }
                match read_global(library.data.colors) {
                    Some(value) => encode_int(value),
                    None => attribute_error(name),
                }
            }
            "COLOR_PAIRS" => {
                if !START_COLOR_CALLED.load(Ordering::Relaxed) {
                    return attribute_error(name);
                }
                match read_global(library.data.color_pairs) {
                    Some(value) => encode_int(value),
                    None => attribute_error(name),
                }
            }
            _ => attribute_error(name),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// ncurses keeps process-global terminal state, so tests that touch it
        /// must not overlap.
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn int(n: i64) -> i64 {
            value::encode_int(n).expect("representable integer")
        }

        fn text(text: &str) -> i64 {
            value::str_alloc(text.to_string())
        }

        fn number(v: i64) -> i64 {
            value::as_integer(v).expect("integer result")
        }

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Opens a PTY pair, or returns `None` when the platform cannot.
        fn open_pty() -> Option<(i32, i32)> {
            // SAFETY: plain descriptor calls with fixed buffers owned here.
            unsafe {
                let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
                if master < 0 {
                    return None;
                }
                if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
                    libc::close(master);
                    return None;
                }
                let mut name = [0 as libc::c_char; 128];
                if libc::ptsname_r(master, name.as_mut_ptr(), name.len()) != 0 {
                    libc::close(master);
                    return None;
                }
                let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                if slave < 0 {
                    libc::close(master);
                    return None;
                }
                Some((master, slave))
            }
        }

        #[test]
        fn library_availability_is_honest() {
            exc::init();
            // On this platform the ncurses library is expected; a missing
            // library skips rather than faking a surface.
            if !available() {
                return;
            }
            let _guard = lock();
            // Calling a real no-argument entry point proves symbol resolution.
            let result = call("isendwin", &[]);
            assert!(!exc::pending(), "isendwin failed");
            assert!(value::type_name(result) == "bool");
        }

        #[test]
        fn constants_match_ncurses() {
            exc::init();
            if !available() {
                return;
            }
            assert_eq!(number(constant("KEY_DOWN")), 258);
            assert_eq!(number(constant("KEY_RESIZE")), 410);
            assert_eq!(number(constant("KEY_MAX")), 511);
            assert_eq!(number(constant("KEY_F1")), 265);
            assert_eq!(number(constant("KEY_F63")), 327);
            assert_eq!(number(constant("A_CHARTEXT")), 255);
            assert_eq!(number(constant("A_ATTRIBUTES")), 4_294_967_040);
            assert_eq!(number(constant("A_BOLD")), 2_097_152);
            assert_eq!(number(constant("A_ITALIC")), 2_147_483_648);
            assert!(!exc::pending());

            // `curses` imports the ACS/size constants eagerly, so the
            // non-interactive fallback is available before `initscr`.
            assert_eq!(number(constant("LINES")), 24);
            assert_eq!(number(constant("COLS")), 80);
            assert_eq!(number(constant("COLORS")), 256);
            assert_eq!(number(constant("COLOR_PAIRS")), 256);
            assert!(value::as_str(constant("ACS_HLINE")).is_some());
            constant("not_a_constant");
            assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
            exc::clear();
        }

        #[test]
        fn rejects_invalid_arguments_without_touching_ncurses() {
            exc::init();
            if !available() {
                return;
            }
            call("initscr", &[int(1)]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("newwin", &[]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("window.addstr", &[int(1), text("x")]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("window.addstr", &[]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("tigetstr", &[]);
            assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
            exc::clear();

            call("keyname", &[int(-1)]);
            // `keyname` requires initscr in CPython, so the state error wins
            // before the argument check.
            assert_eq!(exc::pending_tag(), EXCEPTION as i64);
            exc::clear();
        }

        #[test]
        fn window_and_error_classes_are_stable() {
            exc::init();
            let window = class_value("_curses", "window").expect("window class");
            assert_eq!(class_value("_curses", "window"), Some(window));
            let error = class_value("_curses", "error").expect("error class");
            assert_eq!(class_value("_curses", "error"), Some(error));
            assert!(class_value("_curses", "missing").is_none());
            assert!(value::class_lookup(window, "addstr").is_some());
            assert!(value::class_lookup(window, "getmaxyx").is_some());
        }

        #[test]
        fn setupterm_on_a_dumb_terminal() {
            exc::init();
            if !available() {
                return;
            }
            let _guard = lock();
            let Some((master, slave)) = open_pty() else {
                return;
            };
            let term = text("dumb");
            let fd = int(i64::from(slave));
            let roots = heap::root_values(&[term, fd]);
            call("setupterm", &[term, fd]);
            heap::release_roots(roots);
            assert!(!exc::pending(), "setupterm(dumb) failed");

            // A second setupterm is a no-op, like CPython.
            let term = text("dumb");
            let fd = int(i64::from(slave));
            let roots = heap::root_values(&[term, fd]);
            call("setupterm", &[term, fd]);
            heap::release_roots(roots);
            assert!(!exc::pending());

            let capability = text("colors");
            let roots = heap::root_values(&[capability]);
            let colors = call("tigetnum", &[capability]);
            heap::release_roots(roots);
            assert!(!exc::pending());
            assert!(value::as_integer(colors).is_some());

            let capability = text("cnorm");
            let roots = heap::root_values(&[capability]);
            let flag = call("tigetflag", &[capability]);
            heap::release_roots(roots);
            assert!(!exc::pending());
            assert!(value::as_integer(flag).is_some());

            // SAFETY: both descriptors came from `open_pty` and close once.
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
        }
    }
}

#[cfg(unix)]
#[allow(unused_imports)]
pub(crate) use unix::{available, call, class_value, constant};

#[cfg(not(unix))]
mod portable {
    use crate::exc::{self, ATTRIBUTE_ERROR, IMPORT_ERROR};

    pub(crate) fn available() -> bool {
        false
    }

    pub(crate) fn call(function: &str, _args: &[i64]) -> i64 {
        let _ = function;
        exc::raise_with_message(IMPORT_ERROR, "curses is not available on this platform")
    }

    pub(crate) fn constant(name: &str) -> i64 {
        exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_curses' has no attribute '{name}'"),
        )
    }

    pub(crate) fn class_value(_module: &str, _name: &str) -> Option<i64> {
        None
    }
}

#[cfg(not(unix))]
#[allow(unused_imports)]
pub(crate) use portable::{available, call, class_value, constant};
