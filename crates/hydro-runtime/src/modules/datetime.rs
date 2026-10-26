//! `datetime`: naive UTC datetime values with `now()` and `strftime()`.
//!
//! The module exposes a single singleton object (`datetime.datetime` in
//! CPython), modelled as an instance of the native `datetime` class. Calling
//! `.now()` returns another instance of the same class whose date/time
//! components are stored as integer attributes, so `.strftime(format)` can
//! render them. `strftime` implements the common CPython directives plus the
//! `-`, `_`, `0`, width and `^` modifiers; unknown directives are copied
//! through unchanged.
//!
//! Time is taken from [`SystemTime`] and interpreted as UTC (there is no
//! local-time zone database or `tzinfo` support). Civil-date conversion is the
//! standard days-from-epoch algorithm, so no external crate is needed.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::exc::{self, ATTRIBUTE_ERROR, OVERFLOW_ERROR, TYPE_ERROR};
use crate::value::{self, NONE};

/// Module name used for native function values.
const MODULE: &str = "datetime";

/// Python-visible class name; the singleton and every result share it.
const CLASS_NAME: &str = "datetime";

/// Internal names consumed by the runtime's native dispatch.
const NOW_METHOD: &str = "_datetime_now";
const STRFTIME_METHOD: &str = "_datetime_strftime";

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Microseconds in a second.
const MICROS_PER_SECOND: i64 = 1_000_000;

/// A naive UTC date and time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DateTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

/// Tags `n`, reporting `OverflowError` if it does not fit the tagged integer.
fn encode(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"))
}

/// Builds the native `datetime` class. A fresh class per call mirrors
/// `timeit.Timer`: no heap value is cached in a static and therefore the
/// collector cannot free a stale class.
fn datetime_class() -> i64 {
    let methods = vec![
        (
            "now".to_string(),
            value::native_function_new(MODULE, NOW_METHOD, 1),
        ),
        (
            "strftime".to_string(),
            value::native_function_new(MODULE, STRFTIME_METHOD, 2),
        ),
    ];
    value::class_new(CLASS_NAME.to_string(), NONE, methods)
}

/// Whether `candidate` is an instance of the native `datetime` class.
fn is_datetime(candidate: i64) -> bool {
    value::instance_class_name(candidate) == Some(CLASS_NAME)
}

/// Allocates an instance carrying `dt`'s components.
fn datetime_instance(dt: DateTime) -> i64 {
    let class = datetime_class();
    let instance = value::instance_new(class);
    for (name, component) in [
        ("year", dt.year),
        ("month", i64::from(dt.month)),
        ("day", i64::from(dt.day)),
        ("hour", i64::from(dt.hour)),
        ("minute", i64::from(dt.minute)),
        ("second", i64::from(dt.second)),
        ("microsecond", i64::from(dt.microsecond)),
    ] {
        value::instance_setattr(instance, name.to_string(), encode(component));
    }
    instance
}

/// Reads one integer component from a `datetime` instance.
fn component(instance: i64, name: &str) -> Option<i64> {
    value::as_integer(value::instance_attr(instance, name)?)
}

/// Reconstructs a [`DateTime`] from an instance, rejecting out-of-range
/// attributes (they are writable from Python).
fn datetime_from_instance(instance: i64) -> Option<DateTime> {
    let dt = DateTime {
        year: component(instance, "year")?,
        month: u32::try_from(component(instance, "month")?).ok()?,
        day: u32::try_from(component(instance, "day")?).ok()?,
        hour: u32::try_from(component(instance, "hour")?).ok()?,
        minute: u32::try_from(component(instance, "minute")?).ok()?,
        second: u32::try_from(component(instance, "second")?).ok()?,
        microsecond: u32::try_from(component(instance, "microsecond")?).ok()?,
    };
    if !(1..=9_999).contains(&dt.year)
        || !(1..=12).contains(&dt.month)
        || !(1..=31).contains(&dt.day)
        || dt.hour > 23
        || dt.minute > 59
        || dt.second > 59
        || dt.microsecond > 999_999
    {
        return None;
    }
    Some(dt)
}

/// Days since 1970-01-01 for a proleptic Gregorian civil date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Civil date for a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month as u32, day as u32)
}

/// Whether `year` is a leap year in the Gregorian calendar.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days elapsed in `year` before `month` (ignoring leap-day adjustment).
const MONTH_STARTS: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// One-based day of the year.
fn day_of_year(dt: &DateTime) -> u32 {
    let month = dt.month as usize;
    let start = MONTH_STARTS
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or(0);
    start
        + dt.day
        + if month > 2 && is_leap_year(dt.year) {
            1
        } else {
            0
        }
}

/// ISO weekday: Monday is 1, Sunday is 7.
fn iso_weekday(year: i64, month: u32, day: u32) -> u32 {
    // 1970-01-01 was a Thursday (index 3 with Monday = 0).
    let index = (days_from_civil(year, month, day) + 3).rem_euclid(7);
    index as u32 + 1
}

/// Number of ISO weeks (52 or 53) in `year`.
fn iso_weeks_in_year(year: i64) -> u32 {
    let january_first = iso_weekday(year, 1, 1);
    if january_first == 4 || (is_leap_year(year) && january_first == 3) {
        53
    } else {
        52
    }
}

/// ISO year and week number for a civil date.
fn iso_week(dt: &DateTime) -> (i64, u32) {
    let ordinal = i64::from(day_of_year(dt));
    let weekday = i64::from(iso_weekday(dt.year, dt.month, dt.day));
    let week = (ordinal - weekday + 10).div_euclid(7);
    if week < 1 {
        let previous = dt.year - 1;
        (previous, iso_weeks_in_year(previous))
    } else {
        let weeks = i64::from(iso_weeks_in_year(dt.year));
        if week > weeks {
            (dt.year + 1, 1)
        } else {
            (dt.year, week as u32)
        }
    }
}

/// Builds a datetime from a Unix timestamp and a microsecond remainder.
fn from_unix(seconds: i64, microsecond: u32) -> DateTime {
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    DateTime {
        year,
        month,
        day,
        hour: (seconds_of_day / 3_600) as u32,
        minute: ((seconds_of_day % 3_600) / 60) as u32,
        second: (seconds_of_day % 60) as u32,
        microsecond: microsecond.min(999_999),
    }
}

/// Decomposes a [`SystemTime`] into a Unix timestamp and microseconds,
/// handling instants before the epoch.
fn unix_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => (
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
            elapsed.subsec_micros(),
        ),
        Err(error) => {
            let elapsed = error.duration();
            let seconds = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
            let micros = u128::from(elapsed.subsec_nanos()).div_ceil(1_000);
            let total = -(i128::from(seconds) * i128::from(MICROS_PER_SECOND) + micros as i128);
            (
                total.div_euclid(i128::from(MICROS_PER_SECOND)) as i64,
                total.rem_euclid(i128::from(MICROS_PER_SECOND)) as u32,
            )
        }
    }
}

/// Current UTC datetime.
fn now_datetime() -> DateTime {
    let (seconds, microsecond) = unix_parts(SystemTime::now());
    from_unix(seconds, microsecond)
}

/// Seconds since the Unix epoch for `dt`.
fn unix_seconds(dt: &DateTime) -> i64 {
    days_from_civil(dt.year, dt.month, dt.day) * SECONDS_PER_DAY
        + i64::from(dt.hour) * 3_600
        + i64::from(dt.minute) * 60
        + i64::from(dt.second)
}

const WEEKDAY_NAMES: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const WEEKDAY_ABBREVIATIONS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBREVIATIONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// One rendered directive plus its default padding.
struct Piece {
    text: String,
    width: usize,
    pad: char,
}

impl Piece {
    fn padded(text: String, width: usize, pad: char) -> Piece {
        Piece { text, width, pad }
    }

    /// Numeric directive, zero-padded to `width`.
    fn number(value: i64, width: usize) -> Piece {
        Piece::padded(value.to_string(), width, '0')
    }
}

/// Renders one directive (without flags or width), or `None` when the
/// directive is unsupported.
fn directive(dt: &DateTime, conversion: char) -> Option<Piece> {
    let weekday = iso_weekday(dt.year, dt.month, dt.day) as usize - 1;
    let piece = match conversion {
        '%' => Piece::padded("%".to_string(), 0, ' '),
        'n' => Piece::padded("\n".to_string(), 0, ' '),
        't' => Piece::padded("\t".to_string(), 0, ' '),
        'Y' => Piece::number(dt.year, 4),
        'y' => Piece::number(dt.year.rem_euclid(100), 2),
        'C' => Piece::number(dt.year.div_euclid(100), 2),
        'm' => Piece::number(i64::from(dt.month), 2),
        'd' => Piece::number(i64::from(dt.day), 2),
        'e' => Piece::padded(dt.day.to_string(), 2, ' '),
        'H' => Piece::number(i64::from(dt.hour), 2),
        'k' => Piece::padded(dt.hour.to_string(), 2, ' '),
        'I' => {
            let hour = i64::from(dt.hour % 12);
            Piece::number(if hour == 0 { 12 } else { hour }, 2)
        }
        'l' => {
            let hour = i64::from(dt.hour % 12);
            Piece::padded(if hour == 0 { 12 } else { hour }.to_string(), 2, ' ')
        }
        'M' => Piece::number(i64::from(dt.minute), 2),
        'S' => Piece::number(i64::from(dt.second), 2),
        'f' => Piece::number(i64::from(dt.microsecond), 6),
        'p' => Piece::padded(if dt.hour < 12 { "AM" } else { "PM" }.to_string(), 0, ' '),
        'P' => Piece::padded(if dt.hour < 12 { "am" } else { "pm" }.to_string(), 0, ' '),
        'j' => Piece::number(i64::from(day_of_year(dt)), 3),
        'a' => Piece::padded(WEEKDAY_ABBREVIATIONS[weekday].to_string(), 0, ' '),
        'A' => Piece::padded(WEEKDAY_NAMES[weekday].to_string(), 0, ' '),
        'w' => Piece::padded(((weekday + 1) % 7).to_string(), 0, ' '),
        'u' => Piece::padded((weekday + 1).to_string(), 0, ' '),
        'b' | 'h' => Piece::padded(
            MONTH_ABBREVIATIONS[dt.month as usize - 1].to_string(),
            0,
            ' ',
        ),
        'B' => Piece::padded(MONTH_NAMES[dt.month as usize - 1].to_string(), 0, ' '),
        'z' => Piece::padded("+0000".to_string(), 0, ' '),
        'Z' => Piece::padded("UTC".to_string(), 0, ' '),
        's' => Piece::number(unix_seconds(dt), 0),
        'V' => Piece::number(i64::from(iso_week(dt).1), 2),
        'G' => Piece::number(iso_week(dt).0, 4),
        'g' => Piece::number(iso_week(dt).0.rem_euclid(100), 2),
        'U' => {
            let sunday = (weekday + 1) % 7;
            let week = (day_of_year(dt) as i64 + 6 - sunday as i64).div_euclid(7);
            Piece::number(week, 2)
        }
        'W' => {
            let week = (day_of_year(dt) as i64 + 6 - weekday as i64).div_euclid(7);
            Piece::number(week, 2)
        }
        'c' => Piece::padded(format(dt, "%a %b %e %H:%M:%S %Y"), 0, ' '),
        'x' => Piece::padded(format(dt, "%m/%d/%y"), 0, ' '),
        'X' => Piece::padded(format(dt, "%H:%M:%S"), 0, ' '),
        'D' => Piece::padded(format(dt, "%m/%d/%y"), 0, ' '),
        'F' => Piece::padded(format(dt, "%Y-%m-%d"), 0, ' '),
        'T' => Piece::padded(format(dt, "%H:%M:%S"), 0, ' '),
        'R' => Piece::padded(format(dt, "%H:%M"), 0, ' '),
        _ => return None,
    };
    Some(piece)
}

/// Upper bound on an explicit `strftime` field width. CPython would honour a
/// larger width, but an unbounded value turns a short format string into a
/// multi-gigabyte allocation, so the renderer clamps instead.
const MAX_FIELD_WIDTH: usize = 4_096;

/// Pads `text` to `width` using `pad`.
fn padded(text: &str, width: usize, pad: char) -> String {
    let width = width.min(MAX_FIELD_WIDTH);
    let length = text.chars().count();
    if length >= width {
        return text.to_string();
    }
    let mut out = String::with_capacity(width);
    for _ in length..width {
        out.push(pad);
    }
    out.push_str(text);
    out
}

/// Renders a datetime with a `strftime`-style format.
fn format(dt: &DateTime, format: &str) -> String {
    let mut out = String::with_capacity(format.len() + 8);
    let mut rest = format;
    while let Some(character) = rest.chars().next() {
        if character != '%' {
            out.push(character);
            rest = &rest[character.len_utf8()..];
            continue;
        }
        let after_percent = &rest[1..];
        rest = after_percent;
        let mut no_pad = false;
        let mut pad: Option<char> = None;
        let mut uppercase = false;
        // Flags precede an optional width.
        loop {
            match rest.chars().next() {
                Some('-') => {
                    no_pad = true;
                    rest = &rest[1..];
                }
                Some('_') => {
                    pad = Some(' ');
                    rest = &rest[1..];
                }
                Some('0') => {
                    pad = Some('0');
                    rest = &rest[1..];
                }
                Some('^') => {
                    uppercase = true;
                    rest = &rest[1..];
                }
                _ => break,
            }
        }
        let mut width: Option<usize> = None;
        let mut explicit_width = 0usize;
        let mut has_width = false;
        while let Some(digit) = rest.chars().next().and_then(|c| c.to_digit(10)) {
            explicit_width = explicit_width
                .saturating_mul(10)
                .saturating_add(digit as usize);
            has_width = true;
            rest = &rest[1..];
        }
        if has_width {
            width = Some(explicit_width);
        }
        // `E` and `O` select locale alternatives; the naive implementation
        // treats them as the plain directive.
        if let Some(character) = rest.chars().next() {
            if character == 'E' || character == 'O' {
                rest = &rest[1..];
            }
        }
        let Some(conversion) = rest.chars().next() else {
            out.push('%');
            break;
        };
        rest = &rest[conversion.len_utf8()..];
        match directive(dt, conversion) {
            Some(piece) => {
                let width = if no_pad {
                    0
                } else {
                    width.unwrap_or(piece.width)
                };
                let pad = pad.unwrap_or(piece.pad);
                let mut text = padded(&piece.text, width, pad);
                if uppercase {
                    text = text.to_uppercase();
                }
                out.push_str(&text);
            }
            None => {
                // Unsupported directive: reproduce it verbatim.
                out.push('%');
                if no_pad {
                    out.push('-');
                }
                if let Some(pad) = pad {
                    out.push(pad);
                }
                if let Some(width) = width {
                    out.push_str(&width.to_string());
                }
                out.push(conversion);
            }
        }
    }
    out
}

/// `datetime.now()`; the receiver is ignored beyond being a datetime.
fn now_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "now() missing required argument: 'self'");
    };
    if !is_datetime(*receiver) {
        return exc::raise_with_message(TYPE_ERROR, "now() must be called on a datetime object");
    }
    datetime_instance(now_datetime())
}

/// `datetime.strftime(format)`.
fn strftime_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return exc::raise_with_message(TYPE_ERROR, "strftime() missing required argument: 'self'");
    };
    let Some(format_value) = args.get(1).copied() else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "strftime() missing required argument: 'format'",
        );
    };
    let Some(pattern) = value::as_str(format_value) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "strftime() argument 'format' must be str, not '{}'",
                value::type_name(format_value)
            ),
        );
    };
    let Some(dt) = datetime_from_instance(receiver) else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "strftime() receiver is not a valid datetime object",
        );
    };
    // Copy the pattern out before allocating the result: the borrowed string
    // must not be read after an allocation that may collect.
    let pattern = pattern.to_string();
    value::str_alloc(format(&dt, &pattern))
}

/// Dispatches a native `datetime` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        NOW_METHOD => now_method(args),
        STRFTIME_METHOD => strftime_method(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'datetime' has no attribute '{function}'"),
        ),
    }
}

/// Module constants. `datetime.datetime` is a fresh singleton object per
/// lookup; `now()` allocates its own result, so no class or instance is cached
/// across collector runs.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "datetime" => datetime_instance(now_datetime()),
        "MINYEAR" => encode(1),
        "MAXYEAR" => encode(9_999),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'datetime' has no attribute '{name}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> i64 {
        value::str_alloc(raw.to_string())
    }

    fn sample() -> DateTime {
        from_unix(1_700_000_000, 123_456)
    }

    #[test]
    fn civil_conversion_round_trips_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(
            days_from_civil(2000, 2, 29),
            days_from_civil(2000, 2, 28) + 1
        );
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn formats_known_timestamp() {
        let dt = sample();
        assert_eq!(format(&dt, "%Y-%m-%d %H:%M:%S"), "2023-11-14 22:13:20");
        assert_eq!(format(&dt, "%Y%m%d-%H%M%S"), "20231114-221320");
        assert_eq!(format(&dt, "%f"), "123456");
        assert_eq!(format(&dt, "%a %A"), "Tue Tuesday");
        assert_eq!(format(&dt, "%b %B"), "Nov November");
        assert_eq!(format(&dt, "%j"), "318");
        assert_eq!(format(&dt, "%I %p"), "10 PM");
        assert_eq!(format(&dt, "%z %Z"), "+0000 UTC");
        assert_eq!(format(&dt, "%s"), "1700000000");
        assert_eq!(format(&dt, "%V %G"), "46 2023");
        assert_eq!(format(&dt, "%U %W"), "46 46");
        assert_eq!(format(&dt, "%c"), "Tue Nov 14 22:13:20 2023");
        assert_eq!(format(&dt, "%F %T"), "2023-11-14 22:13:20");
        assert_eq!(format(&dt, "%%n"), "%n");
    }

    #[test]
    fn formats_epoch_and_flags() {
        let epoch = from_unix(0, 0);
        assert_eq!(
            format(&epoch, "%Y-%m-%dT%H:%M:%S.%f"),
            "1970-01-01T00:00:00.000000"
        );
        assert_eq!(format(&epoch, "%a %j %p %w %u"), "Thu 001 AM 4 4");
        assert_eq!(format(&epoch, "%-d/%_m/%0H"), "1/ 1/00");
        assert_eq!(format(&epoch, "%^a"), "THU");
        assert_eq!(format(&epoch, "%Q"), "%Q");
        assert_eq!(format(&epoch, "100%"), "100%");
        // A pathological width is clamped instead of exhausting memory.
        assert_eq!(format(&epoch, "%5000Y").len(), MAX_FIELD_WIDTH);
    }

    #[test]
    fn formats_leap_day_and_iso_boundaries() {
        let leap = from_unix(days_from_civil(2000, 2, 29) * SECONDS_PER_DAY, 0);
        assert_eq!(format(&leap, "%Y-%m-%d %j"), "2000-02-29 060");
        assert_eq!(format(&leap, "%V %G"), "09 2000");

        // 2021-01-01 belongs to ISO week 53 of 2020.
        let new_year = from_unix(days_from_civil(2021, 1, 1) * SECONDS_PER_DAY, 0);
        assert_eq!(format(&new_year, "%V %G"), "53 2020");

        // 2016-01-01 belongs to ISO week 53 of 2015.
        let before = from_unix(days_from_civil(2016, 1, 1) * SECONDS_PER_DAY, 0);
        assert_eq!(format(&before, "%V %G"), "53 2015");
    }

    #[test]
    fn pre_epoch_timestamps_decompose() {
        let dt = from_unix(-1, 500_000);
        assert_eq!(
            format(&dt, "%Y-%m-%d %H:%M:%S.%f"),
            "1969-12-31 23:59:59.500000"
        );
        let (seconds, micros) = unix_parts(UNIX_EPOCH - std::time::Duration::new(1, 500_000_000));
        assert_eq!((seconds, micros), (-2, 500_000));
    }

    #[test]
    fn singleton_exposes_now_and_strftime() {
        exc::init();
        let singleton = constant("datetime");
        assert!(!exc::pending());
        assert!(is_datetime(singleton));
        let class = value::as_instance(singleton).expect("instance").class;
        assert_eq!(value::class_display_name(class), Some(CLASS_NAME));
        assert!(value::class_method(class, "now").is_some());
        assert!(value::class_method(class, "strftime").is_some());
    }

    #[test]
    fn now_returns_fresh_datetime_instances() {
        exc::init();
        let singleton = constant("datetime");
        let first = call(NOW_METHOD, &[singleton]);
        let second = call(NOW_METHOD, &[singleton]);
        assert!(!exc::pending());
        assert!(is_datetime(first));
        assert!(is_datetime(second));
        assert_ne!(first, second);
        let year = component(first, "year").expect("year");
        assert!(year >= 2020);
        assert!(value::instance_attr(first, "month").is_some());
        assert!(value::instance_attr(first, "microsecond").is_some());

        // The rendered timestamp must be a well-formed date string.
        let rendered = call(STRFTIME_METHOD, &[first, text("%Y%m%d-%H%M%S")]);
        let rendered = value::as_str(rendered).expect("string");
        assert_eq!(rendered.len(), 15);
        assert_eq!(&rendered[8..9], "-");
        assert!(rendered[..8].bytes().all(|byte| byte.is_ascii_digit()));
    }

    #[test]
    fn strftime_reads_components_from_instances() {
        exc::init();
        let instance = datetime_instance(sample());
        let rendered = call(STRFTIME_METHOD, &[instance, text("%Y-%m-%dT%H:%M:%S.%f")]);
        assert!(!exc::pending());
        assert_eq!(value::as_str(rendered), Some("2023-11-14T22:13:20.123456"));
    }

    #[test]
    fn invalid_arguments_report_errors() {
        exc::init();
        call(STRFTIME_METHOD, &[]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        let instance = datetime_instance(sample());
        call(STRFTIME_METHOD, &[instance]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        call(
            STRFTIME_METHOD,
            &[instance, value::encode_int(1).expect("int")],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        call("missing", &[]);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));

        exc::init();
        constant("missing");
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));

        exc::init();
        call(NOW_METHOD, &[]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }

    #[test]
    fn mutated_components_are_rejected() {
        exc::init();
        let instance = datetime_instance(sample());
        value::instance_setattr(
            instance,
            "month".to_string(),
            value::encode_int(13).expect("int"),
        );
        assert!(datetime_from_instance(instance).is_none());
        call(STRFTIME_METHOD, &[instance, text("%Y")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }
}
