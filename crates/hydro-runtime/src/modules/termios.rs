//! `termios`: the Linux terminal interface.
//!
//! The module mirrors CPython's `termios` on Linux: the same constant names
//! (`IGNBRK`...`EXTPROC`, the `B*` baud rates as bits per second, the
//! `V*` control-character indices, `NCCS`, the `TCS*`/`TC*` actions and the
//! common `ioctl` numbers), the same seven-element attribute list from
//! [`tcgetattr`](call) (`iflag`, `oflag`, `cflag`, `lflag`, `ispeed`, `ospeed`,
//! `cc`), and the same `termios.error` class for kernel failures. Errors carry
//! CPython's `(errno, 'message')` text.
//!
//! Descriptors are accepted as integers (including negative validation) or as
//! objects exposing `fileno()`, exactly like CPython's
//! `PyObject_AsFileDescriptor`. `tcgetattr`/`tcsetattr` work on any terminal
//! descriptor; the PTY-based inline test opens its own pair and skips cleanly
//! when the platform cannot provide one.
//!
//! The constants are Linux (asm-generic) values. Modern glibc (2.42+)
//! reports the `B*` constants as the actual baud rate, which is what CPython
//! exposes and what `tcgetattr` returns in `c_ispeed`/`c_ospeed`; the
//! historically encoded C values (`B9600 == 0o15`) are deliberately not used.

use std::io;
use std::mem;

use crate::core::methods;
use crate::exc::{self, ATTRIBUTE_ERROR, EXCEPTION, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::value::{self, NONE};

fn encode_int(n: i64) -> i64 {
    value::encode_int(n).unwrap_or(NONE)
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
        &format!("module 'termios' has no attribute '{name}'"),
    )
}

/// `(name, value)` pairs exposed by `termios`: CPython 3.14's Linux values
/// for the names this module publishes.
const TERMIOS_CONSTANTS: &[(&str, i64)] = &[
    // Input flags.
    ("IGNBRK", 1),
    ("BRKINT", 2),
    ("IGNPAR", 4),
    ("PARMRK", 8),
    ("INPCK", 16),
    ("ISTRIP", 32),
    ("INLCR", 64),
    ("IGNCR", 128),
    ("ICRNL", 256),
    ("IUCLC", 512),
    ("IXON", 1024),
    ("IXANY", 2048),
    ("IXOFF", 4096),
    ("IMAXBEL", 8192),
    ("IUTF8", 16384),
    // Output flags.
    ("OPOST", 1),
    ("OLCUC", 2),
    ("ONLCR", 4),
    ("OCRNL", 8),
    ("ONOCR", 16),
    ("ONLRET", 32),
    ("OFILL", 64),
    ("OFDEL", 128),
    ("NLDLY", 256),
    ("NL0", 0),
    ("NL1", 256),
    ("CRDLY", 1536),
    ("CR0", 0),
    ("CR1", 512),
    ("CR2", 1024),
    ("CR3", 1536),
    ("TABDLY", 6144),
    ("TAB0", 0),
    ("TAB1", 2048),
    ("TAB2", 4096),
    ("TAB3", 6144),
    ("XTABS", 6144),
    ("BSDLY", 8192),
    ("BS0", 0),
    ("BS1", 8192),
    ("VTDLY", 16384),
    ("VT0", 0),
    ("VT1", 16384),
    ("FFDLY", 32768),
    ("FF0", 0),
    ("FF1", 32768),
    // Control flags.
    ("CBAUD", 4111),
    ("CBAUDEX", 4096),
    ("CIBAUD", 269418496),
    ("CSIZE", 48),
    ("CS5", 0),
    ("CS6", 16),
    ("CS7", 32),
    ("CS8", 48),
    ("CSTOPB", 64),
    ("CREAD", 128),
    ("PARENB", 256),
    ("PARODD", 512),
    ("HUPCL", 1024),
    ("CLOCAL", 2048),
    ("CRTSCTS", 2147483648),
    // Local flags.
    ("ISIG", 1),
    ("ICANON", 2),
    ("XCASE", 4),
    ("ECHO", 8),
    ("ECHOE", 16),
    ("ECHOK", 32),
    ("ECHONL", 64),
    ("NOFLSH", 128),
    ("TOSTOP", 256),
    ("ECHOCTL", 512),
    ("ECHOPRT", 1024),
    ("ECHOKE", 2048),
    ("FLUSHO", 4096),
    ("PENDIN", 16384),
    ("IEXTEN", 32768),
    ("EXTPROC", 65536),
    // Control-character indices.
    ("VINTR", 0),
    ("VQUIT", 1),
    ("VERASE", 2),
    ("VKILL", 3),
    ("VEOF", 4),
    ("VTIME", 5),
    ("VMIN", 6),
    ("VSWTC", 7),
    ("VSWTCH", 7),
    ("VSTART", 8),
    ("VSTOP", 9),
    ("VSUSP", 10),
    ("VEOL", 11),
    ("VREPRINT", 12),
    ("VDISCARD", 13),
    ("VWERASE", 14),
    ("VLNEXT", 15),
    ("VEOL2", 16),
    // Control-character values.
    ("CINTR", 3),
    ("CQUIT", 28),
    ("CERASE", 127),
    ("CKILL", 21),
    ("CEOF", 4),
    ("CEOL", 0),
    ("CEOT", 4),
    ("CSTART", 17),
    ("CSTOP", 19),
    ("CSUSP", 26),
    ("CDSUSP", 25),
    ("CRPRNT", 18),
    ("CWERASE", 23),
    ("CFLUSH", 15),
    ("CLNEXT", 22),
    ("_POSIX_VDISABLE", 0),
    // Baud rates, in bits per second (glibc 2.42 / CPython 3.14 values).
    ("B0", 0),
    ("B50", 50),
    ("B75", 75),
    ("B110", 110),
    ("B134", 134),
    ("B150", 150),
    ("B200", 200),
    ("B300", 300),
    ("B600", 600),
    ("B1200", 1200),
    ("B1800", 1800),
    ("B2400", 2400),
    ("B4800", 4800),
    ("B9600", 9600),
    ("B19200", 19200),
    ("B38400", 38400),
    ("EXTA", 19200),
    ("EXTB", 38400),
    ("B57600", 57600),
    ("B115200", 115200),
    ("B230400", 230400),
    ("B460800", 460800),
    ("B500000", 500000),
    ("B576000", 576000),
    ("B921600", 921600),
    ("B1000000", 1000000),
    ("B1152000", 1152000),
    ("B1500000", 1500000),
    ("B2000000", 2000000),
    ("B2500000", 2500000),
    ("B3000000", 3000000),
    ("B3500000", 3500000),
    ("B4000000", 4000000),
    ("B7200", 7200),
    ("B14400", 14400),
    ("B28800", 28800),
    ("B76800", 76800),
    // Buffer size and line discipline.
    ("NCCS", libc::NCCS as i64),
    ("N_TTY", 0),
    ("N_SLIP", 1),
    ("N_MOUSE", 2),
    ("N_PPP", 3),
    ("N_STRIP", 4),
    // `tcsetattr` actions.
    ("TCSANOW", 0),
    ("TCSADRAIN", 1),
    ("TCSAFLUSH", 2),
    // `tcflush`/`tcflow` queues and actions.
    ("TCIFLUSH", 0),
    ("TCOFLUSH", 1),
    ("TCIOFLUSH", 2),
    ("TCOOFF", 0),
    ("TCOON", 1),
    ("TCIOFF", 2),
    ("TCION", 3),
    // Common termios ioctls.
    ("TCGETS", 21505),
    ("TCSETS", 21506),
    ("TCSETSW", 21507),
    ("TCSETSF", 21508),
    ("TCGETA", 21509),
    ("TCSETA", 21510),
    ("TCSETAW", 21511),
    ("TCSETAF", 21512),
    ("TCSBRK", 21513),
    ("TCXONC", 21514),
    ("TCFLSH", 21515),
    ("TCSBRKP", 21541),
    ("TIOCEXCL", 21516),
    ("TIOCNXCL", 21517),
    ("TIOCSCTTY", 21518),
    ("TIOCGPGRP", 21519),
    ("TIOCSPGRP", 21520),
    ("TIOCOUTQ", 21521),
    ("TIOCSTI", 21522),
    ("TIOCGWINSZ", 21523),
    ("TIOCSWINSZ", 21524),
    ("TIOCNOTTY", 21538),
    ("TIOCMGET", 21525),
    ("TIOCMSET", 21528),
    ("FIONREAD", 21531),
    ("TIOCINQ", 21531),
    ("FIONBIO", 21537),
    ("FIOCLEX", 21585),
    ("FIONCLEX", 21584),
    ("FIOASYNC", 21586),
];

/// `termios.error`: a plain `Exception` subclass, as in CPython (not an
/// `OSError`).
fn error_class() -> i64 {
    const KEY: &str = "termios.error";
    if let Some(class) = crate::core::globals::registry_get(KEY) {
        return class;
    }
    let class = value::class_new(
        "error".to_string(),
        exc::exception_class(EXCEPTION),
        Vec::new(),
    );
    crate::core::globals::registry_set(KEY, class);
    class
}

fn strerror(code: i32) -> String {
    let text = io::Error::from_raw_os_error(code).to_string();
    let detail = text.split(" (os error").next().unwrap_or(text.as_str());
    detail.to_string()
}

/// Raises `class(message)` and returns the `None` dummy value.
fn raise_class_error(class: i64, message: &str) -> i64 {
    let message = value::str_alloc(message.to_string());
    let roots = heap::root_values(&[message, class]);
    let instance = crate::builtins::call(class, 1, &message as *const i64);
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

/// Reports the current `errno` as `termios.error`, using CPython's
/// `(errno, 'message')` rendering.
fn termios_failure() -> i64 {
    let error = io::Error::last_os_error();
    let code = error.raw_os_error().unwrap_or(libc::EINVAL);
    raise_class_error(error_class(), &format!("({code}, '{}')", strerror(code)))
}

/// The C `int` conversion used for `when`, `queue`, `action` and `duration`.
fn c_int_argument(v: i64) -> Result<i32, i64> {
    match value::as_integer(v) {
        Some(n) if (i32::MIN as i64..=i32::MAX as i64).contains(&n) => Ok(n as i32),
        Some(_) => Err(overflow_error("Python int too large to convert to C int")),
        None if value::type_name(v) == "int" => {
            Err(overflow_error("Python int too large to convert to C int"))
        }
        None => Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))),
    }
}

/// The C `long` conversion used for the first six attribute-list entries.
fn long_argument(v: i64) -> Result<i64, i64> {
    match value::as_integer(v) {
        Some(n) => Ok(n),
        None if value::type_name(v) == "int" => {
            Err(overflow_error("Python int too large to convert to C long"))
        }
        None => Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        ))),
    }
}

/// Resolves a descriptor from an integer or an object with `fileno()`.
fn file_descriptor(v: i64) -> Result<i32, i64> {
    let type_name = value::type_name(v);
    if type_name == "int" || type_name == "bool" {
        let Some(fd) = value::as_integer(v) else {
            return Err(overflow_error("Python int too large to convert to C int"));
        };
        if fd < 0 {
            return Err(value_error(&format!(
                "file descriptor cannot be a negative integer ({fd})"
            )));
        }
        return i32::try_from(fd)
            .map_err(|_| overflow_error("Python int too large to convert to C int"));
    }
    let method = methods::getattr(v, b"fileno".as_ptr(), b"fileno".len());
    if exc::pending() {
        exc::clear();
        return Err(type_error(
            "argument must be an int, or have a fileno() method.",
        ));
    }
    let result = crate::builtins::call(method, 0, std::ptr::null());
    if exc::pending() {
        return Err(NONE);
    }
    let result_type = value::type_name(result);
    if result_type == "int" || result_type == "bool" {
        let Some(fd) = value::as_integer(result) else {
            return Err(overflow_error("Python int too large to convert to C int"));
        };
        if fd < 0 {
            return Err(value_error(&format!(
                "file descriptor cannot be a negative integer ({fd})"
            )));
        }
        return i32::try_from(fd)
            .map_err(|_| overflow_error("Python int too large to convert to C int"));
    }
    Err(type_error("fileno() returned a non-integer"))
}

/// One `cc` entry: a one-byte `bytes` object or an integer.
fn control_byte(v: i64) -> Result<libc::cc_t, i64> {
    const MESSAGE: &str =
        "tcsetattr: elements of attributes must be bytes objects of length 1 or integers";
    if let Some(bytes) = value::as_bytes(v) {
        return match bytes.len() {
            1 => Ok(bytes[0]),
            _ => Err(type_error(MESSAGE)),
        };
    }
    let type_name = value::type_name(v);
    if type_name == "int" || type_name == "bool" {
        let Some(number) = value::as_integer(v) else {
            return Err(overflow_error("Python int too large to convert to C long"));
        };
        return Ok(number as libc::cc_t);
    }
    Err(type_error(MESSAGE))
}

/// `tcgetattr(fd)`: the seven-element attribute list.
fn tcgetattr_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "termios.tcgetattr() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let mut attributes: libc::termios = unsafe { mem::zeroed() };
    // SAFETY: `attributes` is a valid output buffer for `tcgetattr` and `fd`
    // was validated as a descriptor above.
    if unsafe { libc::tcgetattr(fd, &mut attributes) } != 0 {
        return termios_failure();
    }
    // The `cc` entries live only in this Rust-side vector until the list
    // takes ownership, so root them while the loop allocates.
    let mark = heap::root_values(&[]);
    let mut controls = Vec::with_capacity(libc::NCCS);
    for &byte in attributes.c_cc.iter() {
        let item = value::bytes_new(vec![byte]);
        heap::root_values(&[item]);
        controls.push(item);
    }
    let controls = value::list_new(controls);
    let result = value::list_new(vec![
        encode_int(i64::from(attributes.c_iflag)),
        encode_int(i64::from(attributes.c_oflag)),
        encode_int(i64::from(attributes.c_cflag)),
        encode_int(i64::from(attributes.c_lflag)),
        encode_int(i64::from(attributes.c_ispeed)),
        encode_int(i64::from(attributes.c_ospeed)),
        controls,
    ]);
    heap::release_roots(mark);
    result
}

/// `tcsetattr(fd, when, attributes)`.
fn tcsetattr_call(args: &[i64]) -> i64 {
    if args.len() != 3 {
        return type_error(&format!(
            "tcsetattr expected 3 arguments, got {}",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let when = match c_int_argument(args[1]) {
        Ok(when) => when,
        Err(error) => return error,
    };
    let Some(values) = value::as_list(args[2]) else {
        return type_error("tcsetattr, arg 3: must be 7 element list");
    };
    if values.len() != 7 {
        return type_error("tcsetattr, arg 3: must be 7 element list");
    }
    let mut fields = [0i64; 6];
    for (slot, &item) in fields.iter_mut().zip(values.iter()) {
        *slot = match long_argument(item) {
            Ok(number) => number,
            Err(error) => return error,
        };
    }
    let mut attributes: libc::termios = unsafe { mem::zeroed() };
    attributes.c_iflag = fields[0] as libc::tcflag_t;
    attributes.c_oflag = fields[1] as libc::tcflag_t;
    attributes.c_cflag = fields[2] as libc::tcflag_t;
    attributes.c_lflag = fields[3] as libc::tcflag_t;
    attributes.c_ispeed = fields[4] as libc::speed_t;
    attributes.c_ospeed = fields[5] as libc::speed_t;
    let Some(controls) = value::as_list(values[6]) else {
        return type_error(&format!(
            "tcsetattr: attributes[6] must be {} element list",
            libc::NCCS
        ));
    };
    if controls.len() != libc::NCCS {
        return type_error(&format!(
            "tcsetattr: attributes[6] must be {} element list",
            libc::NCCS
        ));
    }
    for (slot, &item) in attributes.c_cc.iter_mut().zip(controls.iter()) {
        *slot = match control_byte(item) {
            Ok(byte) => byte,
            Err(error) => return error,
        };
    }
    // SAFETY: every field of `attributes` was filled in above and `fd` was
    // validated; the kernel validates `when`.
    if unsafe { libc::tcsetattr(fd, when, &attributes) } != 0 {
        return termios_failure();
    }
    NONE
}

/// `tcsendbreak(fd, duration)`.
fn tcsendbreak_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "tcsendbreak expected 2 arguments, got {}",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let duration = match c_int_argument(args[1]) {
        Ok(duration) => duration,
        Err(error) => return error,
    };
    // SAFETY: `fd` was validated; the kernel handles non-terminal error paths.
    if unsafe { libc::tcsendbreak(fd, duration) } != 0 {
        return termios_failure();
    }
    NONE
}

/// `tcdrain(fd)`.
fn tcdrain_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "termios.tcdrain() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    // SAFETY: `fd` was validated; `tcdrain` only reads the descriptor.
    if unsafe { libc::tcdrain(fd) } != 0 {
        return termios_failure();
    }
    NONE
}

/// `tcflush(fd, queue)`.
fn tcflush_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!("tcflush expected 2 arguments, got {}", args.len()));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let queue = match c_int_argument(args[1]) {
        Ok(queue) => queue,
        Err(error) => return error,
    };
    // SAFETY: `fd` was validated; the kernel validates `queue`.
    if unsafe { libc::tcflush(fd, queue) } != 0 {
        return termios_failure();
    }
    NONE
}

/// `tcflow(fd, action)`.
fn tcflow_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!("tcflow expected 2 arguments, got {}", args.len()));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let action = match c_int_argument(args[1]) {
        Ok(action) => action,
        Err(error) => return error,
    };
    // SAFETY: `fd` was validated; the kernel validates `action`.
    if unsafe { libc::tcflow(fd, action) } != 0 {
        return termios_failure();
    }
    NONE
}

/// `tcgetwinsize(fd)`: the `(rows, columns)` tuple.
fn tcgetwinsize_call(args: &[i64]) -> i64 {
    if args.len() != 1 {
        return type_error(&format!(
            "termios.tcgetwinsize() takes exactly one argument ({} given)",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let mut size: libc::winsize = unsafe { mem::zeroed() };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through the valid pointer.
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size as *mut libc::winsize) };
    if result != 0 {
        return termios_failure();
    }
    value::tuple_new(vec![
        encode_int(i64::from(size.ws_row)),
        encode_int(i64::from(size.ws_col)),
    ])
}

/// One `tcsetwinsize` element.
fn winsize_value(v: i64) -> Result<libc::c_ushort, i64> {
    let Some(number) = value::as_integer(v) else {
        if value::type_name(v) == "int" {
            return Err(overflow_error("Python int too large to convert to C long"));
        }
        return Err(type_error(&format!(
            "'{}' object cannot be interpreted as an integer",
            value::type_name(v)
        )));
    };
    if !(0..=i64::from(u16::MAX)).contains(&number) {
        return Err(overflow_error("winsize value(s) out of range."));
    }
    Ok(number as libc::c_ushort)
}

/// `tcsetwinsize(fd, (rows, columns))`.
fn tcsetwinsize_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return type_error(&format!(
            "tcsetwinsize expected 2 arguments, got {}",
            args.len()
        ));
    }
    let fd = match file_descriptor(args[0]) {
        Ok(fd) => fd,
        Err(error) => return error,
    };
    let Some(items) = value::as_list(args[1]).or_else(|| value::as_tuple(args[1])) else {
        return type_error("tcsetwinsize, arg 2: must be a two-item sequence");
    };
    if items.len() != 2 {
        return type_error("tcsetwinsize, arg 2: must be a two-item sequence");
    }
    let rows = match winsize_value(items[0]) {
        Ok(rows) => rows,
        Err(error) => return error,
    };
    let columns = match winsize_value(items[1]) {
        Ok(columns) => columns,
        Err(error) => return error,
    };
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one initialised `winsize` through the valid
    // pointer; `fd` was validated above.
    let result = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size as *const libc::winsize) };
    if result != 0 {
        return termios_failure();
    }
    NONE
}

/// Materialises a module constant.
pub(crate) fn constant(name: &str) -> i64 {
    if let Some(entry) = TERMIOS_CONSTANTS.iter().find(|entry| entry.0 == name) {
        return encode_int(entry.1);
    }
    match name {
        "error" => error_class(),
        _ => attribute_error(name),
    }
}

/// Dispatches a native `termios` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "tcgetattr" => tcgetattr_call(args),
        "tcsetattr" => tcsetattr_call(args),
        "tcsendbreak" => tcsendbreak_call(args),
        "tcdrain" => tcdrain_call(args),
        "tcflush" => tcflush_call(args),
        "tcflow" => tcflow_call(args),
        "tcgetwinsize" => tcgetwinsize_call(args),
        "tcsetwinsize" => tcsetwinsize_call(args),
        _ => attribute_error(function),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable integer")
    }

    fn number(value_arg: i64) -> i64 {
        value::as_integer(value_arg).expect("integer result")
    }

    /// Opens a PTY pair, or returns `None` when the platform cannot provide
    /// one (the test then skips cleanly).
    #[cfg(unix)]
    fn open_pty() -> Option<(i32, i32)> {
        // SAFETY: every call below uses plain descriptors and fixed-size
        // buffers owned by this frame; failures are reported, never assumed.
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
    fn constants_match_linux_termios() {
        exc::init();
        assert_eq!(number(constant("NCCS")), i64::from(libc::NCCS as u32));
        assert_eq!(number(constant("B9600")), 9600);
        assert_eq!(number(constant("B38400")), 38400);
        assert_eq!(number(constant("IGNBRK")), 1);
        assert_eq!(number(constant("OPOST")), 1);
        assert_eq!(number(constant("ECHO")), 8);
        assert_eq!(number(constant("CS8")), 48);
        assert_eq!(number(constant("VMIN")), 6);
        assert_eq!(number(constant("VTIME")), 5);
        assert_eq!(number(constant("VDISCARD")), 13);
        assert_eq!(number(constant("TCSANOW")), 0);
        assert_eq!(number(constant("TCSAFLUSH")), 2);
        assert_eq!(number(constant("TCIOFLUSH")), 2);
        assert_eq!(number(constant("TIOCGWINSZ")), 21523);
        assert_eq!(number(constant("TIOCSWINSZ")), 21524);
        assert!(!exc::pending());
    }

    #[test]
    fn rejects_invalid_descriptors_and_attributes() {
        exc::init();

        call("tcgetattr", &[int(-1)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("tcgetattr", &[value::str_alloc("x".to_string())]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call("tcgetattr", &[]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        call(
            "tcsetattr",
            &[int(0), int(0), value::str_alloc("x".to_string())],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call(
            "tcsetattr",
            &[int(0), int(0), value::list_new(vec![int(0); 6])],
        );
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();

        // An unknown `when` reaches the kernel and is reported as
        // `termios.error` (EINVAL on a terminal, ENOTTY otherwise).
        let controls = value::list_new(vec![int(0); libc::NCCS]);
        let attributes = value::list_new(vec![
            int(0),
            int(0),
            int(0),
            int(0),
            int(0),
            int(0),
            controls,
        ]);
        call("tcsetattr", &[int(0), int(99), attributes]);
        assert_eq!(exc::pending_tag(), EXCEPTION as i64);
        exc::clear();

        call("tcflush", &[int(-1), int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("tcflow", &[int(-1), int(0)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();

        call("tcsetwinsize", &[int(0), value::list_new(vec![int(1)])]);
        assert_eq!(exc::pending_tag(), TYPE_ERROR as i64);
        exc::clear();
        call(
            "tcsetwinsize",
            &[int(0), value::list_new(vec![int(70_000), int(1)])],
        );
        assert_eq!(exc::pending_tag(), OVERFLOW_ERROR as i64);
    }

    #[test]
    fn pty_round_trip() {
        exc::init();
        let Some((master, slave)) = open_pty() else {
            return;
        };
        let fd = int(i64::from(slave));

        let attributes = call("tcgetattr", &[fd]);
        assert!(!exc::pending(), "tcgetattr failed on a PTY");
        let values = value::as_list(attributes).expect("7 element list").to_vec();
        assert_eq!(values.len(), 7);
        let roots = heap::root_values(&values);

        let controls = value::as_list(values[6]).expect("cc list");
        assert_eq!(controls.len(), libc::NCCS);
        assert_eq!(
            value::as_bytes(controls[0]).map(|bytes| bytes.len()),
            Some(1)
        );

        // Clearing ECHO must be visible in a fresh tcgetattr.
        let lflag = value::as_integer(values[3]).expect("lflag");
        let mut updated = values.clone();
        updated[3] = int(lflag & !8);
        call("tcsetattr", &[fd, int(0), value::list_new(updated)]);
        assert!(!exc::pending(), "tcsetattr failed on a PTY");
        let reread = call("tcgetattr", &[fd]);
        assert!(!exc::pending());
        let reread = value::as_list(reread).expect("list");
        assert_eq!(value::as_integer(reread[3]).expect("lflag") & 8, 0);

        call(
            "tcsetwinsize",
            &[fd, value::tuple_new(vec![int(24), int(80)])],
        );
        assert!(!exc::pending());
        let winsize = call("tcgetwinsize", &[fd]);
        assert!(!exc::pending());
        let winsize = value::as_tuple(winsize).expect("two-item tuple");
        assert_eq!(value::as_integer(winsize[0]), Some(24));
        assert_eq!(value::as_integer(winsize[1]), Some(80));

        call("tcdrain", &[fd]);
        assert!(!exc::pending());
        call("tcflush", &[fd, int(2)]);
        assert!(!exc::pending());
        call("tcflow", &[fd, int(1)]);
        assert!(!exc::pending());
        call("tcsendbreak", &[fd, int(0)]);
        assert!(!exc::pending());

        // The kernel rejects an unknown `when` with EINVAL, which is reported
        // as `termios.error` (an Exception subclass).
        call("tcsetattr", &[fd, int(99), value::list_new(values)]);
        assert_eq!(exc::pending_tag(), EXCEPTION as i64);
        exc::clear();

        heap::release_roots(roots);
        // SAFETY: both descriptors came from `open_pty` and are closed once.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }
}
