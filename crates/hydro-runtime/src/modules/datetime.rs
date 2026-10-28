//! `datetime`: `date`, `time`, `datetime`, `timedelta` and `tzinfo`.
//!
//! The five value classes are cached native classes (registered under
//! `datetime.*` in the runtime registry, so repeated module lookups and
//! `type()`/`isinstance()` identity agree). Every instance stores its
//! components as integer attributes, which keeps attribute reads, `vars()` and
//! the `zoneinfo` interop working. Constructors validate their arguments with
//! CPython's ranges and report the usual `TypeError`/`ValueError`/
//! `OverflowError` wording; keyword calls (`timedelta(days=1)`,
//! `datetime(2020, 1, 2, tzinfo=...)`) bind the declared parameter names.
//!
//! Supported arithmetic: `date`/`datetime` ± `timedelta`,
//! `date - date`, `datetime - datetime` and `timedelta ± timedelta`. The
//! runtime's `+`/`-` operators consult instance dunders, so these work
//! directly; `timedelta * int` and unary `-`/`abs()` are not available because
//! the runtime's `*` and unary operators do not dispatch class dunders.
//!
//! `strftime` implements the common CPython directives plus the `-`, `_`, `0`,
//! width and `^` modifiers; unknown directives are copied through unchanged.
//!
//! Time is taken from [`SystemTime`] and interpreted as UTC. A `tzinfo` value
//! is stored and consulted for `utcoffset`/`isoformat`/`timestamp`, but
//! `now(tz)` attaches the zone without converting the wall clock, because the
//! runtime has no local-time database. Civil-date conversion is the standard
//! days-from-epoch algorithm, so no external crate is needed.

use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use hydro_ir::MethodKind;

use crate::core::globals;
use crate::exc::{self, ATTRIBUTE_ERROR, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

/// Module name used for native function values.
const MODULE: &str = "datetime";

/// Python-visible class names and their registry keys.
const DATE_CLASS: &str = "date";
const TIME_CLASS: &str = "time";
const DATETIME_CLASS: &str = "datetime";
const TIMEDELTA_CLASS: &str = "timedelta";
const TZINFO_CLASS: &str = "tzinfo";
const DATE_KEY: &str = "datetime.date";
const TIME_KEY: &str = "datetime.time";
const DATETIME_KEY: &str = "datetime.datetime";
const TIMEDELTA_KEY: &str = "datetime.timedelta";
const TZINFO_KEY: &str = "datetime.tzinfo";

/// Internal names consumed by the runtime's native dispatch.
const NOW_METHOD: &str = "_datetime_now";
const UTCNOW_METHOD: &str = "_datetime_utcnow";
const DATE_TODAY_METHOD: &str = "_date_today";
const TODAY_METHOD: &str = "_datetime_today";
const FROMTIMESTAMP_METHOD: &str = "_datetime_fromtimestamp";
const COMBINE_METHOD: &str = "_datetime_combine";
const STRFTIME_METHOD: &str = "_datetime_strftime";
const ISOFORMAT_METHOD: &str = "_datetime_isoformat";
const DATE_METHOD: &str = "_datetime_date";
const TIME_METHOD: &str = "_datetime_time";
const TIMESTAMP_METHOD: &str = "_datetime_timestamp";
const WEEKDAY_METHOD: &str = "_datetime_weekday";
const ISOWEEKDAY_METHOD: &str = "_datetime_isoweekday";
const TOORDINAL_METHOD: &str = "_datetime_toordinal";
const TOTAL_SECONDS_METHOD: &str = "_datetime_total_seconds";
const REPLACE_METHOD: &str = "_datetime_replace";
const UTCOFFSET_METHOD: &str = "_datetime_utcoffset";
const DST_METHOD: &str = "_datetime_dst";
const TZNAME_METHOD: &str = "_datetime_tzname";
const TZINFO_UTCOFFSET_METHOD: &str = "_datetime_tzinfo_utcoffset";
const TZINFO_DST_METHOD: &str = "_datetime_tzinfo_dst";
const TZINFO_TZNAME_METHOD: &str = "_datetime_tzinfo_tzname";
const REPR_METHOD: &str = "_datetime_repr";
const STR_METHOD: &str = "_datetime_str";
const EQ_METHOD: &str = "_datetime_eq";
const LT_METHOD: &str = "_datetime_lt";
const LE_METHOD: &str = "_datetime_le";
const GT_METHOD: &str = "_datetime_gt";
const GE_METHOD: &str = "_datetime_ge";
const ADD_METHOD: &str = "_datetime_add";
const RADDMETHOD: &str = "_datetime_radd";
const SUB_METHOD: &str = "_datetime_sub";

/// `datetime.MINYEAR`/`datetime.MAXYEAR`.
const MINYEAR: i64 = 1;
const MAXYEAR: i64 = 9_999;

/// Days between 0001-01-01 (ordinal 1) and 1970-01-01 (`toordinal()` 719_163).
const ORDINAL_EPOCH_OFFSET: i64 = 719_162;

/// Seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Microseconds in a second.
const MICROS_PER_SECOND: i64 = 1_000_000;

/// Microseconds in a day.
const MICROS_PER_DAY: i128 = 86_400_000_000;

/// CPython's `timedelta` magnitude limit for the normalized day count.
const MAX_TIMEDELTA_DAYS: i128 = 999_999_999;

/// Comparison selector shared by the rich-comparison dunders.
const OP_EQ: u8 = 0;
const OP_LT: u8 = 1;
const OP_LE: u8 = 2;
const OP_GT: u8 = 3;
const OP_GE: u8 = 4;

// -------------------------------------------------------------------------
// Value shapes
// -------------------------------------------------------------------------

/// Which of the module's classes an instance belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Date,
    Time,
    DateTime,
    Timedelta,
    TzInfo,
}

/// A calendar date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Date {
    year: i64,
    month: u32,
    day: u32,
}

/// A wall-clock time of day (no zone).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Time {
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

/// A date and wall-clock time, used by the renderer and the arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Parts {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
}

/// A duration stored as normalized total microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Delta(i128);

impl Delta {
    /// Whole days, rounded towards negative infinity.
    fn days(self) -> i64 {
        self.0.div_euclid(MICROS_PER_DAY) as i64
    }

    /// Seconds within the day (always `0..=86399`, like CPython, so negative
    /// durations borrow from the day count instead of carrying a sign).
    fn seconds(self) -> i64 {
        (self.0.rem_euclid(MICROS_PER_DAY) / i128::from(MICROS_PER_SECOND)) as i64
    }

    /// Microseconds within the second (always `0..=999999`).
    fn microseconds(self) -> i64 {
        self.0.rem_euclid(i128::from(MICROS_PER_SECOND)) as i64
    }

    fn total_seconds(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

// -------------------------------------------------------------------------
// Small helpers
// -------------------------------------------------------------------------

/// Tags `n`, reporting `OverflowError` if it does not fit the tagged integer.
fn encode(n: i64) -> i64 {
    value::encode_int(n)
        .unwrap_or_else(|| exc::raise_with_message(OVERFLOW_ERROR, "integer overflow"))
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

fn arity_error(function: &str, expected: &str, given: usize) -> i64 {
    exc::raise_with_message(
        TYPE_ERROR,
        &format!("{function}() takes {expected} argument(s) ({given} given)"),
    )
}

fn invalid_instance(message: &str) -> i64 {
    exc::raise_with_message(VALUE_ERROR, message)
}

/// Reads one integer component from an instance.
fn component(instance: i64, name: &str) -> Option<i64> {
    value::as_integer(value::instance_attr(instance, name)?)
}

/// Which class `instance` belongs to; `None` for anything else.
fn kind_of(instance: i64) -> Option<Kind> {
    match value::instance_class_name(instance)? {
        "date" => Some(Kind::Date),
        "time" => Some(Kind::Time),
        "datetime" => Some(Kind::DateTime),
        "timedelta" => Some(Kind::Timedelta),
        "tzinfo" => Some(Kind::TzInfo),
        _ => None,
    }
}

/// Stored `tzinfo` attribute (`None` when absent).
fn tzinfo_of(instance: i64) -> i64 {
    value::instance_attr(instance, "tzinfo").unwrap_or(NONE)
}

/// Stored `fold` attribute (`0` when absent).
fn fold_of(instance: i64) -> i64 {
    component(instance, "fold").unwrap_or(0)
}

/// Whether `year` is a leap year in the Gregorian calendar.
fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in a month (0 for an out-of-range month number).
fn days_in_month(year: i64, month: u32) -> u32 {
    const LENGTHS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) {
        return 0;
    }
    let days = LENGTHS[(month - 1) as usize];
    if month == 2 && is_leap_year(year) {
        29
    } else {
        days
    }
}

fn date_is_valid(year: i64, month: i64, day: i64) -> bool {
    (MINYEAR..=MAXYEAR).contains(&year)
        && (1..=12).contains(&month)
        && (1..=i64::from(days_in_month(year, month as u32))).contains(&day)
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

/// Days elapsed in `year` before `month` (ignoring leap-day adjustment).
const MONTH_STARTS: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// One-based day of the year.
fn day_of_year(parts: &Parts) -> u32 {
    let month = parts.month as usize;
    let start = MONTH_STARTS
        .get(month.saturating_sub(1))
        .copied()
        .unwrap_or(0);
    start
        + parts.day
        + if month > 2 && is_leap_year(parts.year) {
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
fn iso_week(parts: &Parts) -> (i64, u32) {
    let ordinal = i64::from(day_of_year(parts));
    let weekday = i64::from(iso_weekday(parts.year, parts.month, parts.day));
    let week = (ordinal - weekday + 10).div_euclid(7);
    if week < 1 {
        let previous = parts.year - 1;
        (previous, iso_weeks_in_year(previous))
    } else {
        let weeks = i64::from(iso_weeks_in_year(parts.year));
        if week > weeks {
            (parts.year + 1, 1)
        } else {
            (parts.year, week as u32)
        }
    }
}

// -------------------------------------------------------------------------
// Class construction
// -------------------------------------------------------------------------

/// Builds one method entry for a native class namespace.
fn entry(
    name: &str,
    native: &'static str,
    arity: usize,
    kind: MethodKind,
) -> (String, i64, MethodKind) {
    (
        name.to_string(),
        value::native_function_new(MODULE, native, arity),
        kind,
    )
}

/// Builds a class from its method table.
///
/// The table is passed as a slice pointing at a stack array, so the freshly
/// allocated method values are conservatively visible to the collector while
/// the earlier entries are still being created. The values are also rooted
/// across the class allocation itself.
fn build_class(name: &str, base: i64, methods: &[(String, i64, MethodKind)]) -> i64 {
    let roots: Vec<i64> = methods.iter().map(|(_, function, _)| *function).collect();
    let roots = crate::heap::root_values(&roots);
    let class = value::class_new_full(name.to_string(), base, methods.to_vec(), Vec::new());
    crate::heap::release_roots(roots);
    class
}

/// Adds one CPython class attribute (`min`, `max` or `resolution`).
///
/// The value is published through the class overlay's write barrier as soon
/// as it exists, so building the three bounds one at a time keeps each one
/// reachable before the next allocation can collect.
fn define_bound(class: i64, name: &str, bound: i64) {
    value::class_set_attr(class, name.to_string(), bound);
}

/// The cached `datetime.date` class.
fn date_class() -> i64 {
    if let Some(class) = globals::registry_get(DATE_KEY) {
        return class;
    }
    let methods = [
        entry("strftime", STRFTIME_METHOD, 2, MethodKind::Normal),
        entry("isoformat", ISOFORMAT_METHOD, 1, MethodKind::Normal),
        entry("weekday", WEEKDAY_METHOD, 1, MethodKind::Normal),
        entry("isoweekday", ISOWEEKDAY_METHOD, 1, MethodKind::Normal),
        entry("toordinal", TOORDINAL_METHOD, 1, MethodKind::Normal),
        entry("replace", REPLACE_METHOD, 1, MethodKind::Normal),
        entry("today", DATE_TODAY_METHOD, 0, MethodKind::Staticmethod),
        entry("__repr__", REPR_METHOD, 1, MethodKind::Normal),
        entry("__str__", STR_METHOD, 1, MethodKind::Normal),
        entry("__eq__", EQ_METHOD, 2, MethodKind::Normal),
        entry("__lt__", LT_METHOD, 2, MethodKind::Normal),
        entry("__le__", LE_METHOD, 2, MethodKind::Normal),
        entry("__gt__", GT_METHOD, 2, MethodKind::Normal),
        entry("__ge__", GE_METHOD, 2, MethodKind::Normal),
        entry("__add__", ADD_METHOD, 2, MethodKind::Normal),
        entry("__radd__", RADDMETHOD, 2, MethodKind::Normal),
        entry("__sub__", SUB_METHOD, 2, MethodKind::Normal),
    ];
    let class = build_class(DATE_CLASS, NONE, &methods);
    globals::registry_set(DATE_KEY, class);
    define_bound(
        class,
        "min",
        new_date(Date {
            year: MINYEAR,
            month: 1,
            day: 1,
        }),
    );
    define_bound(
        class,
        "max",
        new_date(Date {
            year: MAXYEAR,
            month: 12,
            day: 31,
        }),
    );
    define_bound(class, "resolution", new_timedelta(Delta(MICROS_PER_DAY)));
    class
}

/// The cached `datetime.time` class.
fn time_class() -> i64 {
    if let Some(class) = globals::registry_get(TIME_KEY) {
        return class;
    }
    let methods = [
        entry("strftime", STRFTIME_METHOD, 2, MethodKind::Normal),
        entry("isoformat", ISOFORMAT_METHOD, 1, MethodKind::Normal),
        entry("replace", REPLACE_METHOD, 1, MethodKind::Normal),
        entry("utcoffset", UTCOFFSET_METHOD, 1, MethodKind::Normal),
        entry("dst", DST_METHOD, 1, MethodKind::Normal),
        entry("tzname", TZNAME_METHOD, 1, MethodKind::Normal),
        entry("__repr__", REPR_METHOD, 1, MethodKind::Normal),
        entry("__str__", STR_METHOD, 1, MethodKind::Normal),
        entry("__eq__", EQ_METHOD, 2, MethodKind::Normal),
        entry("__lt__", LT_METHOD, 2, MethodKind::Normal),
        entry("__le__", LE_METHOD, 2, MethodKind::Normal),
        entry("__gt__", GT_METHOD, 2, MethodKind::Normal),
        entry("__ge__", GE_METHOD, 2, MethodKind::Normal),
    ];
    let class = build_class(TIME_CLASS, NONE, &methods);
    globals::registry_set(TIME_KEY, class);
    define_bound(
        class,
        "min",
        new_time(
            Time {
                hour: 0,
                minute: 0,
                second: 0,
                microsecond: 0,
            },
            NONE,
            0,
        ),
    );
    define_bound(
        class,
        "max",
        new_time(
            Time {
                hour: 23,
                minute: 59,
                second: 59,
                microsecond: 999_999,
            },
            NONE,
            0,
        ),
    );
    define_bound(class, "resolution", new_timedelta(Delta(1)));
    class
}

/// The cached `datetime.datetime` class; its base is `date`, matching
/// CPython's `isinstance(datetime_obj, date)`.
fn datetime_class() -> i64 {
    if let Some(class) = globals::registry_get(DATETIME_KEY) {
        return class;
    }
    let methods = [
        entry("now", NOW_METHOD, 1, MethodKind::Staticmethod),
        entry("utcnow", UTCNOW_METHOD, 0, MethodKind::Staticmethod),
        entry("today", TODAY_METHOD, 0, MethodKind::Staticmethod),
        entry(
            "fromtimestamp",
            FROMTIMESTAMP_METHOD,
            2,
            MethodKind::Staticmethod,
        ),
        entry("combine", COMBINE_METHOD, 2, MethodKind::Staticmethod),
        entry("strftime", STRFTIME_METHOD, 2, MethodKind::Normal),
        entry("isoformat", ISOFORMAT_METHOD, 2, MethodKind::Normal),
        entry("date", DATE_METHOD, 1, MethodKind::Normal),
        entry("time", TIME_METHOD, 1, MethodKind::Normal),
        entry("timestamp", TIMESTAMP_METHOD, 1, MethodKind::Normal),
        entry("weekday", WEEKDAY_METHOD, 1, MethodKind::Normal),
        entry("isoweekday", ISOWEEKDAY_METHOD, 1, MethodKind::Normal),
        entry("toordinal", TOORDINAL_METHOD, 1, MethodKind::Normal),
        entry("replace", REPLACE_METHOD, 1, MethodKind::Normal),
        entry("utcoffset", UTCOFFSET_METHOD, 1, MethodKind::Normal),
        entry("dst", DST_METHOD, 1, MethodKind::Normal),
        entry("tzname", TZNAME_METHOD, 1, MethodKind::Normal),
        entry("__repr__", REPR_METHOD, 1, MethodKind::Normal),
        entry("__str__", STR_METHOD, 1, MethodKind::Normal),
        entry("__eq__", EQ_METHOD, 2, MethodKind::Normal),
        entry("__lt__", LT_METHOD, 2, MethodKind::Normal),
        entry("__le__", LE_METHOD, 2, MethodKind::Normal),
        entry("__gt__", GT_METHOD, 2, MethodKind::Normal),
        entry("__ge__", GE_METHOD, 2, MethodKind::Normal),
        entry("__add__", ADD_METHOD, 2, MethodKind::Normal),
        entry("__radd__", RADDMETHOD, 2, MethodKind::Normal),
        entry("__sub__", SUB_METHOD, 2, MethodKind::Normal),
    ];
    let class = build_class(DATETIME_CLASS, date_class(), &methods);
    globals::registry_set(DATETIME_KEY, class);
    define_bound(
        class,
        "min",
        new_datetime(
            Date {
                year: MINYEAR,
                month: 1,
                day: 1,
            },
            Time {
                hour: 0,
                minute: 0,
                second: 0,
                microsecond: 0,
            },
            NONE,
            0,
        ),
    );
    define_bound(
        class,
        "max",
        new_datetime(
            Date {
                year: MAXYEAR,
                month: 12,
                day: 31,
            },
            Time {
                hour: 23,
                minute: 59,
                second: 59,
                microsecond: 999_999,
            },
            NONE,
            0,
        ),
    );
    define_bound(class, "resolution", new_timedelta(Delta(1)));
    class
}

/// The cached `datetime.timedelta` class.
fn timedelta_class() -> i64 {
    if let Some(class) = globals::registry_get(TIMEDELTA_KEY) {
        return class;
    }
    let methods = [
        entry("total_seconds", TOTAL_SECONDS_METHOD, 1, MethodKind::Normal),
        entry("__repr__", REPR_METHOD, 1, MethodKind::Normal),
        entry("__str__", STR_METHOD, 1, MethodKind::Normal),
        entry("__eq__", EQ_METHOD, 2, MethodKind::Normal),
        entry("__lt__", LT_METHOD, 2, MethodKind::Normal),
        entry("__le__", LE_METHOD, 2, MethodKind::Normal),
        entry("__gt__", GT_METHOD, 2, MethodKind::Normal),
        entry("__ge__", GE_METHOD, 2, MethodKind::Normal),
        entry("__add__", ADD_METHOD, 2, MethodKind::Normal),
        entry("__radd__", RADDMETHOD, 2, MethodKind::Normal),
        entry("__sub__", SUB_METHOD, 2, MethodKind::Normal),
    ];
    let class = build_class(TIMEDELTA_CLASS, NONE, &methods);
    globals::registry_set(TIMEDELTA_KEY, class);
    let max = Delta(
        MAX_TIMEDELTA_DAYS * MICROS_PER_DAY + 86_399 * i128::from(MICROS_PER_SECOND) + 999_999,
    );
    define_bound(
        class,
        "min",
        new_timedelta(Delta(-MAX_TIMEDELTA_DAYS * MICROS_PER_DAY)),
    );
    define_bound(class, "max", new_timedelta(max));
    define_bound(class, "resolution", new_timedelta(Delta(1)));
    class
}

/// The cached `datetime.tzinfo` base class. Its methods return `None`, like
/// CPython's base class, so a bare `tzinfo()` is usable as an "unknown zone".
fn tzinfo_class() -> i64 {
    if let Some(class) = globals::registry_get(TZINFO_KEY) {
        return class;
    }
    let methods = [
        entry("utcoffset", TZINFO_UTCOFFSET_METHOD, 2, MethodKind::Normal),
        entry("dst", TZINFO_DST_METHOD, 2, MethodKind::Normal),
        entry("tzname", TZINFO_TZNAME_METHOD, 2, MethodKind::Normal),
        entry("__repr__", REPR_METHOD, 1, MethodKind::Normal),
        entry("__str__", STR_METHOD, 1, MethodKind::Normal),
    ];
    let class = build_class(TZINFO_CLASS, NONE, &methods);
    globals::registry_set(TZINFO_KEY, class);
    class
}

/// Which of the module's classes `class` is, from the registry only (never
/// creating a class).
fn registered_kind(class: i64) -> Option<Kind> {
    if globals::registry_get(DATE_KEY) == Some(class) {
        return Some(Kind::Date);
    }
    if globals::registry_get(TIME_KEY) == Some(class) {
        return Some(Kind::Time);
    }
    if globals::registry_get(DATETIME_KEY) == Some(class) {
        return Some(Kind::DateTime);
    }
    if globals::registry_get(TIMEDELTA_KEY) == Some(class) {
        return Some(Kind::Timedelta);
    }
    if globals::registry_get(TZINFO_KEY) == Some(class) {
        return Some(Kind::TzInfo);
    }
    None
}

// -------------------------------------------------------------------------
// Instance construction
// -------------------------------------------------------------------------

fn new_date(date: Date) -> i64 {
    let class = date_class();
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "year".to_string(), encode(date.year));
    value::instance_setattr(instance, "month".to_string(), encode(i64::from(date.month)));
    value::instance_setattr(instance, "day".to_string(), encode(i64::from(date.day)));
    instance
}

fn new_time(time: Time, tzinfo: i64, fold: i64) -> i64 {
    let class = time_class();
    let instance = value::instance_new(class);
    write_time(instance, time);
    value::instance_setattr(instance, "tzinfo".to_string(), tzinfo);
    value::instance_setattr(instance, "fold".to_string(), encode(fold));
    instance
}

fn new_datetime(date: Date, time: Time, tzinfo: i64, fold: i64) -> i64 {
    let class = datetime_class();
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "year".to_string(), encode(date.year));
    value::instance_setattr(instance, "month".to_string(), encode(i64::from(date.month)));
    value::instance_setattr(instance, "day".to_string(), encode(i64::from(date.day)));
    write_time(instance, time);
    value::instance_setattr(instance, "tzinfo".to_string(), tzinfo);
    value::instance_setattr(instance, "fold".to_string(), encode(fold));
    instance
}

fn write_time(instance: i64, time: Time) {
    value::instance_setattr(instance, "hour".to_string(), encode(i64::from(time.hour)));
    value::instance_setattr(
        instance,
        "minute".to_string(),
        encode(i64::from(time.minute)),
    );
    value::instance_setattr(
        instance,
        "second".to_string(),
        encode(i64::from(time.second)),
    );
    value::instance_setattr(
        instance,
        "microsecond".to_string(),
        encode(i64::from(time.microsecond)),
    );
}

fn new_timedelta(delta: Delta) -> i64 {
    let class = timedelta_class();
    let instance = value::instance_new(class);
    value::instance_setattr(instance, "days".to_string(), encode(delta.days()));
    value::instance_setattr(instance, "seconds".to_string(), encode(delta.seconds()));
    value::instance_setattr(
        instance,
        "microseconds".to_string(),
        encode(delta.microseconds()),
    );
    instance
}

/// Reads a validated `Date` from a `date`/`datetime` instance.
fn date_from_instance(instance: i64) -> Option<Date> {
    match kind_of(instance)? {
        Kind::Date | Kind::DateTime => {
            let year = component(instance, "year")?;
            let month = component(instance, "month")?;
            let day = component(instance, "day")?;
            if !date_is_valid(year, month, day) {
                return None;
            }
            Some(Date {
                year,
                month: month as u32,
                day: day as u32,
            })
        }
        _ => None,
    }
}

/// Reads a validated `Time` from a `time`/`datetime` instance.
fn time_from_instance(instance: i64) -> Option<Time> {
    match kind_of(instance)? {
        Kind::Time | Kind::DateTime => {
            let hour = component(instance, "hour")?;
            let minute = component(instance, "minute")?;
            let second = component(instance, "second")?;
            let microsecond = component(instance, "microsecond")?;
            if !(0..=23).contains(&hour)
                || !(0..=59).contains(&minute)
                || !(0..=59).contains(&second)
                || !(0..=999_999).contains(&microsecond)
            {
                return None;
            }
            Some(Time {
                hour: hour as u32,
                minute: minute as u32,
                second: second as u32,
                microsecond: microsecond as u32,
            })
        }
        _ => None,
    }
}

/// Reads a validated duration from a `timedelta` instance.
fn delta_from_instance(instance: i64) -> Option<Delta> {
    if kind_of(instance) != Some(Kind::Timedelta) {
        return None;
    }
    let days = component(instance, "days")?;
    let seconds = component(instance, "seconds")?;
    let microseconds = component(instance, "microseconds")?;
    if !(0..SECONDS_PER_DAY).contains(&seconds) || !(0..=999_999).contains(&microseconds) {
        return None;
    }
    let total = i128::from(days) * MICROS_PER_DAY
        + i128::from(seconds) * i128::from(MICROS_PER_SECOND)
        + i128::from(microseconds);
    if total.div_euclid(MICROS_PER_DAY).abs() > MAX_TIMEDELTA_DAYS {
        return None;
    }
    Some(Delta(total))
}

/// The renderer's view of a `date`/`time`/`datetime` instance.
fn parts_of(instance: i64) -> Option<Parts> {
    match kind_of(instance)? {
        Kind::Date => {
            let date = date_from_instance(instance)?;
            Some(Parts {
                year: date.year,
                month: date.month,
                day: date.day,
                hour: 0,
                minute: 0,
                second: 0,
                microsecond: 0,
            })
        }
        Kind::DateTime => {
            let date = date_from_instance(instance)?;
            let time = time_from_instance(instance)?;
            Some(Parts {
                year: date.year,
                month: date.month,
                day: date.day,
                hour: time.hour,
                minute: time.minute,
                second: time.second,
                microsecond: time.microsecond,
            })
        }
        // CPython renders a bare `time` against 1900-01-01.
        Kind::Time => {
            let time = time_from_instance(instance)?;
            Some(Parts {
                year: 1900,
                month: 1,
                day: 1,
                hour: time.hour,
                minute: time.minute,
                second: time.second,
                microsecond: time.microsecond,
            })
        }
        _ => None,
    }
}

// -------------------------------------------------------------------------
// Argument validation
// -------------------------------------------------------------------------

/// Slots of `date(year, month, day)`.
const DATE_PARAMS: [&str; 3] = ["year", "month", "day"];

/// Slots of `time(hour=0, minute=0, second=0, microsecond=0, tzinfo=None, *,
/// fold=0)`; `fold` is keyword-only.
const TIME_PARAMS: [&str; 6] = ["hour", "minute", "second", "microsecond", "tzinfo", "fold"];
const TIME_POSITIONAL: usize = 5;

/// Slots of `datetime(year, month, day, hour=0, ..., tzinfo=None, *, fold=0)`.
const DATETIME_PARAMS: [&str; 9] = [
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "second",
    "microsecond",
    "tzinfo",
    "fold",
];
const DATETIME_POSITIONAL: usize = 8;

/// Slots of `timedelta(days=0, seconds=0, microseconds=0, milliseconds=0,
/// minutes=0, hours=0, weeks=0)`.
const TIMEDELTA_PARAMS: [&str; 7] = [
    "days",
    "seconds",
    "microseconds",
    "milliseconds",
    "minutes",
    "hours",
    "weeks",
];

/// Binds positional and keyword arguments into one slot per declared parameter.
fn bind(
    function: &str,
    params: &[&str],
    positional: usize,
    args: &[i64],
    keywords: &[(String, i64)],
) -> Result<Vec<Option<i64>>, i64> {
    if args.len() > positional {
        return Err(arity_error(
            function,
            &format!("at most {positional} positional"),
            args.len(),
        ));
    }
    let mut slots: Vec<Option<i64>> = vec![None; params.len()];
    for (index, argument) in args.iter().enumerate() {
        slots[index] = Some(*argument);
    }
    for (name, argument) in keywords {
        let Some(index) = params.iter().position(|param| *param == name.as_str()) else {
            return Err(type_error(&format!(
                "{function}() got an unexpected keyword argument '{name}'"
            )));
        };
        if slots[index].is_some() {
            return Err(type_error(&format!(
                "{function}() got multiple values for argument '{name}'"
            )));
        }
        slots[index] = Some(*argument);
    }
    Ok(slots)
}

/// Decodes one integer argument.
fn integer_argument(candidate: i64) -> Result<i64, i64> {
    if let Some(number) = value::as_integer(candidate) {
        return Ok(number);
    }
    if value::is_bigint(candidate) {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "Python int too large to convert to C int",
        ));
    }
    Err(type_error(&format!(
        "'{}' object cannot be interpreted as an integer",
        value::type_name(candidate)
    )))
}

/// Decodes a required integer slot.
fn required_integer(slot: Option<i64>, function: &str, name: &str) -> Result<i64, i64> {
    let Some(candidate) = slot else {
        return Err(type_error(&format!(
            "{function}() missing required argument: '{name}'"
        )));
    };
    integer_argument(candidate)
}

/// Decodes an optional integer slot with a default.
fn default_integer(slot: Option<i64>, default: i64) -> Result<i64, i64> {
    match slot {
        Some(candidate) => integer_argument(candidate),
        None => Ok(default),
    }
}

/// Whether `candidate` is a usable `tzinfo` (the module's class or any object
/// that implements `utcoffset`).
fn is_tzinfo(candidate: i64) -> bool {
    let Some(class) = value::instance_class_value(candidate) else {
        return false;
    };
    value::class_display_name(class) == Some(TZINFO_CLASS)
        || value::class_lookup(class, "utcoffset").is_some()
}

fn tzinfo_argument(candidate: i64) -> Result<i64, i64> {
    if candidate == NONE || is_tzinfo(candidate) {
        return Ok(candidate);
    }
    Err(type_error(
        "tzinfo argument must be None or of a tzinfo subclass",
    ))
}

fn fold_argument(slot: Option<i64>) -> Result<i64, i64> {
    let fold = default_integer(slot, 0)?;
    if !(0..=1).contains(&fold) {
        return Err(invalid_instance("fold must be either 0 or 1"));
    }
    Ok(fold)
}

fn validate_date(year: i64, month: i64, day: i64) -> Result<Date, i64> {
    if !(MINYEAR..=MAXYEAR).contains(&year) {
        return Err(invalid_instance(&format!("year {year} is out of range")));
    }
    if !(1..=12).contains(&month) {
        return Err(invalid_instance("month must be in 1..12"));
    }
    let limit = i64::from(days_in_month(year, month as u32));
    if !(1..=limit).contains(&day) {
        return Err(invalid_instance("day is out of range for month"));
    }
    Ok(Date {
        year,
        month: month as u32,
        day: day as u32,
    })
}

fn validate_time(hour: i64, minute: i64, second: i64, microsecond: i64) -> Result<Time, i64> {
    if !(0..=23).contains(&hour) {
        return Err(invalid_instance("hour must be in 0..23"));
    }
    if !(0..=59).contains(&minute) {
        return Err(invalid_instance("minute must be in 0..59"));
    }
    if !(0..=59).contains(&second) {
        return Err(invalid_instance("second must be in 0..59"));
    }
    if !(0..=999_999).contains(&microsecond) {
        return Err(invalid_instance("microsecond must be in 0..999999"));
    }
    Ok(Time {
        hour: hour as u32,
        minute: minute as u32,
        second: second as u32,
        microsecond: microsecond as u32,
    })
}

/// Decodes one `timedelta` component into microseconds.
fn delta_component(candidate: i64, label: &str, scale: i128) -> Result<i128, i64> {
    if let Some(number) = value::as_integer(candidate) {
        return Ok(i128::from(number) * scale);
    }
    if value::is_bigint(candidate) {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "Python int too large to convert to C int",
        ));
    }
    let Some(number) = value::as_float(candidate) else {
        return Err(type_error(&format!(
            "unsupported type for timedelta {label} component: {}",
            value::type_name(candidate)
        )));
    };
    let scaled = number * scale as f64;
    if !scaled.is_finite() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            &format!("timedelta {label} component is out of range"),
        ));
    }
    Ok(scaled.round_ties_even() as i128)
}

fn delta_from_total(total: i128) -> Result<Delta, i64> {
    let days = total.div_euclid(MICROS_PER_DAY);
    if days.abs() > MAX_TIMEDELTA_DAYS {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            &format!("days={days}; must have magnitude <= {}", MAX_TIMEDELTA_DAYS),
        ));
    }
    Ok(Delta(total))
}

// -------------------------------------------------------------------------
// Constructors
// -------------------------------------------------------------------------

fn date_from_slots(slots: &[Option<i64>]) -> Result<Date, i64> {
    let year = required_integer(slots[0], "date", "year")?;
    let month = required_integer(slots[1], "date", "month")?;
    let day = required_integer(slots[2], "date", "day")?;
    validate_date(year, month, day)
}

fn time_from_slots(slots: &[Option<i64>]) -> Result<(Time, i64, i64), i64> {
    let hour = default_integer(slots[0], 0)?;
    let minute = default_integer(slots[1], 0)?;
    let second = default_integer(slots[2], 0)?;
    let microsecond = default_integer(slots[3], 0)?;
    let tzinfo = tzinfo_argument(slots[4].unwrap_or(NONE))?;
    let fold = fold_argument(slots[5])?;
    let time = validate_time(hour, minute, second, microsecond)?;
    Ok((time, tzinfo, fold))
}

fn datetime_from_slots(slots: &[Option<i64>]) -> Result<(Date, Time, i64, i64), i64> {
    let year = required_integer(slots[0], "datetime", "year")?;
    let month = required_integer(slots[1], "datetime", "month")?;
    let day = required_integer(slots[2], "datetime", "day")?;
    let date = validate_date(year, month, day)?;
    let hour = default_integer(slots[3], 0)?;
    let minute = default_integer(slots[4], 0)?;
    let second = default_integer(slots[5], 0)?;
    let microsecond = default_integer(slots[6], 0)?;
    let time = validate_time(hour, minute, second, microsecond)?;
    let tzinfo = tzinfo_argument(slots[7].unwrap_or(NONE))?;
    let fold = fold_argument(slots[8])?;
    Ok((date, time, tzinfo, fold))
}

fn timedelta_from_slots(slots: &[Option<i64>]) -> Result<Delta, i64> {
    let units: [(&str, i128); 7] = [
        ("days", MICROS_PER_DAY),
        ("seconds", i128::from(MICROS_PER_SECOND)),
        ("microseconds", 1),
        ("milliseconds", 1_000),
        ("minutes", 60 * i128::from(MICROS_PER_SECOND)),
        ("hours", 3_600 * i128::from(MICROS_PER_SECOND)),
        ("weeks", 7 * MICROS_PER_DAY),
    ];
    let mut total: i128 = 0;
    for (index, (label, scale)) in units.iter().enumerate() {
        let Some(candidate) = slots[index] else {
            continue;
        };
        total += delta_component(candidate, label, *scale)?;
    }
    delta_from_total(total)
}

fn date_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match bind("date", &DATE_PARAMS, DATE_PARAMS.len(), args, keywords)
        .and_then(|slots| date_from_slots(&slots))
    {
        Ok(date) => new_date(date),
        Err(err) => err,
    }
}

fn time_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match bind("time", &TIME_PARAMS, TIME_POSITIONAL, args, keywords)
        .and_then(|slots| time_from_slots(&slots))
    {
        Ok((time, tzinfo, fold)) => new_time(time, tzinfo, fold),
        Err(err) => err,
    }
}

fn datetime_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match bind(
        "datetime",
        &DATETIME_PARAMS,
        DATETIME_POSITIONAL,
        args,
        keywords,
    )
    .and_then(|slots| datetime_from_slots(&slots))
    {
        Ok((date, time, tzinfo, fold)) => new_datetime(date, time, tzinfo, fold),
        Err(err) => err,
    }
}

fn timedelta_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match bind(
        "timedelta",
        &TIMEDELTA_PARAMS,
        TIMEDELTA_PARAMS.len(),
        args,
        keywords,
    )
    .and_then(|slots| timedelta_from_slots(&slots))
    {
        Ok(delta) => new_timedelta(delta),
        Err(err) => err,
    }
}

fn tzinfo_new(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if !args.is_empty() || !keywords.is_empty() {
        return arity_error("tzinfo", "exactly 0", args.len() + keywords.len());
    }
    value::instance_new(tzinfo_class())
}

/// Runs the constructor of one of the module's value classes, or returns
/// `None` when `class` is not one of them.
pub(crate) fn instantiate(class: i64, args: &[i64]) -> Option<i64> {
    Some(match registered_kind(class)? {
        Kind::Date => date_new(args, &[]),
        Kind::Time => time_new(args, &[]),
        Kind::DateTime => datetime_new(args, &[]),
        Kind::Timedelta => timedelta_new(args, &[]),
        Kind::TzInfo => tzinfo_new(args, &[]),
    })
}

/// Keyword variant of [`instantiate`].
pub(crate) fn instantiate_kw(class: i64, args: &[i64], keywords: &[(String, i64)]) -> Option<i64> {
    Some(match registered_kind(class)? {
        Kind::Date => date_new(args, keywords),
        Kind::Time => time_new(args, keywords),
        Kind::DateTime => datetime_new(args, keywords),
        Kind::Timedelta => timedelta_new(args, keywords),
        Kind::TzInfo => tzinfo_new(args, keywords),
    })
}

// -------------------------------------------------------------------------
// Time sources
// -------------------------------------------------------------------------

/// Builds a datetime from a Unix timestamp and a microsecond remainder.
fn from_unix(seconds: i64, microsecond: u32) -> Parts {
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Parts {
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

/// Current UTC instant.
fn now_parts() -> Parts {
    let (seconds, microsecond) = unix_parts(SystemTime::now());
    from_unix(seconds, microsecond)
}

/// Builds [`Parts`] from a count of microseconds since the Unix epoch.
fn parts_from_micros(total: i128) -> Result<Parts, i64> {
    let days = total.div_euclid(MICROS_PER_DAY);
    let micros_of_day = total.rem_euclid(MICROS_PER_DAY);
    let Ok(days) = i64::try_from(days) else {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timestamp out of range for platform time_t",
        ));
    };
    let (year, month, day) = civil_from_days(days);
    if !(MINYEAR..=MAXYEAR).contains(&year) {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timestamp out of range for platform time_t",
        ));
    }
    let seconds = (micros_of_day / 1_000_000) as i64;
    Ok(Parts {
        year,
        month,
        day,
        hour: (seconds / 3_600) as u32,
        minute: ((seconds % 3_600) / 60) as u32,
        second: (seconds % 60) as u32,
        microsecond: (micros_of_day % 1_000_000) as u32,
    })
}

/// Seconds since the Unix epoch for `parts`, treating it as UTC.
fn unix_seconds(parts: &Parts) -> i64 {
    days_from_civil(parts.year, parts.month, parts.day) * SECONDS_PER_DAY
        + i64::from(parts.hour) * 3_600
        + i64::from(parts.minute) * 60
        + i64::from(parts.second)
}

/// Decodes a `fromtimestamp` argument into microseconds since the epoch.
fn timestamp_micros(candidate: i64) -> Result<i128, i64> {
    if let Some(seconds) = value::as_integer(candidate) {
        return Ok(i128::from(seconds) * 1_000_000);
    }
    if value::is_bigint(candidate) {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timestamp out of range for platform time_t",
        ));
    }
    let Some(seconds) = value::as_float(candidate) else {
        return Err(type_error(&format!(
            "fromtimestamp() argument must be int or float, not {}",
            value::type_name(candidate)
        )));
    };
    let micros = seconds * 1_000_000.0;
    if !micros.is_finite() {
        return Err(exc::raise_with_message(
            OVERFLOW_ERROR,
            "timestamp out of range for platform time_t",
        ));
    }
    Ok(micros.round_ties_even() as i128)
}

// -------------------------------------------------------------------------
// `strftime`
// -------------------------------------------------------------------------

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
fn directive(parts: &Parts, conversion: char) -> Option<Piece> {
    let weekday = iso_weekday(parts.year, parts.month, parts.day) as usize - 1;
    let piece = match conversion {
        '%' => Piece::padded("%".to_string(), 0, ' '),
        'n' => Piece::padded("\n".to_string(), 0, ' '),
        't' => Piece::padded("\t".to_string(), 0, ' '),
        'Y' => Piece::number(parts.year, 4),
        'y' => Piece::number(parts.year.rem_euclid(100), 2),
        'C' => Piece::number(parts.year.div_euclid(100), 2),
        'm' => Piece::number(i64::from(parts.month), 2),
        'd' => Piece::number(i64::from(parts.day), 2),
        'e' => Piece::padded(parts.day.to_string(), 2, ' '),
        'H' => Piece::number(i64::from(parts.hour), 2),
        'k' => Piece::padded(parts.hour.to_string(), 2, ' '),
        'I' => {
            let hour = i64::from(parts.hour % 12);
            Piece::number(if hour == 0 { 12 } else { hour }, 2)
        }
        'l' => {
            let hour = i64::from(parts.hour % 12);
            Piece::padded(if hour == 0 { 12 } else { hour }.to_string(), 2, ' ')
        }
        'M' => Piece::number(i64::from(parts.minute), 2),
        'S' => Piece::number(i64::from(parts.second), 2),
        'f' => Piece::number(i64::from(parts.microsecond), 6),
        'p' => Piece::padded(
            if parts.hour < 12 { "AM" } else { "PM" }.to_string(),
            0,
            ' ',
        ),
        'P' => Piece::padded(
            if parts.hour < 12 { "am" } else { "pm" }.to_string(),
            0,
            ' ',
        ),
        'j' => Piece::number(i64::from(day_of_year(parts)), 3),
        'a' => Piece::padded(WEEKDAY_ABBREVIATIONS[weekday].to_string(), 0, ' '),
        'A' => Piece::padded(WEEKDAY_NAMES[weekday].to_string(), 0, ' '),
        'w' => Piece::padded(((weekday + 1) % 7).to_string(), 0, ' '),
        'u' => Piece::padded((weekday + 1).to_string(), 0, ' '),
        'b' | 'h' => Piece::padded(
            MONTH_ABBREVIATIONS[parts.month as usize - 1].to_string(),
            0,
            ' ',
        ),
        'B' => Piece::padded(MONTH_NAMES[parts.month as usize - 1].to_string(), 0, ' '),
        'z' => Piece::padded("+0000".to_string(), 0, ' '),
        'Z' => Piece::padded("UTC".to_string(), 0, ' '),
        's' => Piece::number(unix_seconds(parts), 0),
        'V' => Piece::number(i64::from(iso_week(parts).1), 2),
        'G' => Piece::number(iso_week(parts).0, 4),
        'g' => Piece::number(iso_week(parts).0.rem_euclid(100), 2),
        'U' => {
            let sunday = (weekday + 1) % 7;
            let week = (day_of_year(parts) as i64 + 6 - sunday as i64).div_euclid(7);
            Piece::number(week, 2)
        }
        'W' => {
            let week = (day_of_year(parts) as i64 + 6 - weekday as i64).div_euclid(7);
            Piece::number(week, 2)
        }
        'c' => Piece::padded(format(parts, "%a %b %e %H:%M:%S %Y"), 0, ' '),
        'x' => Piece::padded(format(parts, "%m/%d/%y"), 0, ' '),
        'X' => Piece::padded(format(parts, "%H:%M:%S"), 0, ' '),
        'D' => Piece::padded(format(parts, "%m/%d/%y"), 0, ' '),
        'F' => Piece::padded(format(parts, "%Y-%m-%d"), 0, ' '),
        'T' => Piece::padded(format(parts, "%H:%M:%S"), 0, ' '),
        'R' => Piece::padded(format(parts, "%H:%M"), 0, ' '),
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
fn format(parts: &Parts, pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut rest = pattern;
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
        match directive(parts, conversion) {
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

// -------------------------------------------------------------------------
// Rendering
// -------------------------------------------------------------------------

/// `±HH:MM` (or `±HH:MM:SS`) for an offset in microseconds.
fn offset_text(offset: i128) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let total_seconds = offset.abs() / i128::from(MICROS_PER_SECOND);
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// `tzinfo.utcoffset(receiver)` as microseconds, `None` when the zone reports
/// no offset.
fn utc_offset(receiver: i64, tzinfo: i64) -> Result<Option<i128>, i64> {
    let name = b"utcoffset";
    let args = [receiver];
    let result = crate::methods::call(tzinfo, name.as_ptr(), name.len(), 1, args.as_ptr());
    if exc::pending() {
        return Err(NONE);
    }
    if result == NONE {
        return Ok(None);
    }
    match delta_from_instance(result) {
        Some(delta) => Ok(Some(delta.0)),
        None => Err(type_error("utcoffset() must return None or a timedelta")),
    }
}

/// The receiver's UTC offset when it carries a `tzinfo`.
fn offset_of(receiver: i64) -> Result<Option<i128>, i64> {
    let tzinfo = tzinfo_of(receiver);
    if tzinfo == NONE {
        return Ok(None);
    }
    utc_offset(receiver, tzinfo)
}

fn date_text(date: Date) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

fn time_text(time: Time, offset: Option<i128>) -> String {
    let mut out = format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second);
    if time.microsecond != 0 {
        out.push_str(&format!(".{:06}", time.microsecond));
    }
    if let Some(offset) = offset {
        out.push_str(&offset_text(offset));
    }
    out
}

fn delta_text(delta: Delta) -> String {
    let days = delta.days();
    let seconds = delta.seconds();
    let micros = delta.microseconds();
    let sign = if days < 0 { "-" } else { "" };
    let magnitude = days.abs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    let mut out = String::new();
    if magnitude != 0 {
        out.push_str(&format!(
            "{magnitude} day{}",
            if magnitude == 1 { "" } else { "s" }
        ));
        out.push_str(", ");
    }
    out.push_str(&format!("{hours}:{minutes:02}:{secs:02}"));
    if micros != 0 {
        out.push_str(&format!(".{micros:06}"));
    }
    format!("{sign}{out}")
}

fn delta_repr(delta: Delta) -> String {
    let days = delta.days();
    let seconds = delta.seconds();
    let micros = delta.microseconds();
    if days == 0 && seconds == 0 && micros == 0 {
        return "datetime.timedelta(0)".to_string();
    }
    let mut out = String::from("datetime.timedelta(");
    let mut first = true;
    for (label, value) in [
        ("days", days),
        ("seconds", seconds),
        ("microseconds", micros),
    ] {
        if value == 0 {
            continue;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(&format!("{label}={value}"));
    }
    out.push(')');
    out
}

fn date_repr(date: Date) -> String {
    format!("datetime.date({}, {}, {})", date.year, date.month, date.day)
}

fn time_repr(time: Time, tzinfo: i64, fold: i64) -> String {
    let mut out = format!("datetime.time({}, {}", time.hour, time.minute);
    if time.second != 0 || time.microsecond != 0 {
        out.push_str(&format!(", {}", time.second));
        if time.microsecond != 0 {
            out.push_str(&format!(", {}", time.microsecond));
        }
    }
    if tzinfo != NONE {
        out.push_str(&format!(", tzinfo={}", value::repr(tzinfo)));
    }
    if fold != 0 {
        out.push_str(&format!(", fold={fold}"));
    }
    out.push(')');
    out
}

fn datetime_repr(date: Date, time: Time, tzinfo: i64, fold: i64) -> String {
    let mut out = format!(
        "datetime.datetime({}, {}, {}, {}, {}",
        date.year, date.month, date.day, time.hour, time.minute
    );
    if time.second != 0 || time.microsecond != 0 {
        out.push_str(&format!(", {}", time.second));
        if time.microsecond != 0 {
            out.push_str(&format!(", {}", time.microsecond));
        }
    }
    if tzinfo != NONE {
        out.push_str(&format!(", tzinfo={}", value::repr(tzinfo)));
    }
    if fold != 0 {
        out.push_str(&format!(", fold={fold}"));
    }
    out.push(')');
    out
}

/// Text of one instance, `None` when it is not a module value.
fn render(candidate: i64, repr: bool) -> Option<String> {
    let kind = kind_of(candidate)?;
    Some(match kind {
        Kind::Date => {
            let date = date_from_instance(candidate)?;
            if repr {
                date_repr(date)
            } else {
                date_text(date)
            }
        }
        Kind::DateTime => {
            let date = date_from_instance(candidate)?;
            let time = time_from_instance(candidate)?;
            if repr {
                datetime_repr(date, time, tzinfo_of(candidate), fold_of(candidate))
            } else {
                let offset = offset_of(candidate).ok().flatten();
                format!("{} {}", date_text(date), time_text(time, offset))
            }
        }
        Kind::Time => {
            let time = time_from_instance(candidate)?;
            if repr {
                time_repr(time, tzinfo_of(candidate), fold_of(candidate))
            } else {
                let offset = offset_of(candidate).ok().flatten();
                time_text(time, offset)
            }
        }
        Kind::Timedelta => {
            let delta = delta_from_instance(candidate)?;
            if repr {
                delta_repr(delta)
            } else {
                delta_text(delta)
            }
        }
        // CPython's base `tzinfo` has no reader-friendly rendering either.
        Kind::TzInfo => "<datetime.tzinfo object>".to_string(),
    })
}

// -------------------------------------------------------------------------
// Comparison and arithmetic
// -------------------------------------------------------------------------

/// Ordering key of a `date`/`datetime` instance (day number plus wall time).
fn instant_key(instance: i64) -> Option<(i64, u32, u32, u32, u32)> {
    let date = date_from_instance(instance)?;
    let time = time_from_instance(instance)?;
    Some((
        days_from_civil(date.year, date.month, date.day),
        time.hour,
        time.minute,
        time.second,
        time.microsecond,
    ))
}

/// Python ordering for two values of the same module class family. Different
/// classes return `None`, which the dunders report as `NotImplemented`.
fn ordering_of(a: i64, b: i64) -> Option<Ordering> {
    let kind = kind_of(a)?;
    if kind_of(b)? != kind {
        return None;
    }
    match kind {
        Kind::Date => {
            let left = date_from_instance(a)?;
            let right = date_from_instance(b)?;
            Some((left.year, left.month, left.day).cmp(&(right.year, right.month, right.day)))
        }
        Kind::DateTime => Some(instant_key(a)?.cmp(&instant_key(b)?)),
        Kind::Time => {
            let left = time_from_instance(a)?;
            let right = time_from_instance(b)?;
            Some(
                (left.hour, left.minute, left.second, left.microsecond).cmp(&(
                    right.hour,
                    right.minute,
                    right.second,
                    right.microsecond,
                )),
            )
        }
        Kind::Timedelta => Some(delta_from_instance(a)?.cmp(&delta_from_instance(b)?)),
        Kind::TzInfo => None,
    }
}

fn compare_method(args: &[i64], op: u8) -> i64 {
    let (Some(left), Some(right)) = (args.first().copied(), args.get(1).copied()) else {
        return arity_error("comparison", "exactly 2", args.len());
    };
    let Some(ordering) = ordering_of(left, right) else {
        return value::NOT_IMPLEMENTED;
    };
    let result = match op {
        OP_EQ => ordering == Ordering::Equal,
        OP_LT => ordering == Ordering::Less,
        OP_LE => ordering != Ordering::Greater,
        OP_GT => ordering == Ordering::Greater,
        _ => ordering != Ordering::Less,
    };
    value::bool_value(result)
}

/// Shifts a `date` by whole days.
fn date_shift(instance: i64, days: i64) -> i64 {
    let Some(date) = date_from_instance(instance) else {
        return invalid_instance("invalid date object");
    };
    let base = days_from_civil(date.year, date.month, date.day);
    let Some(total) = base.checked_add(days) else {
        return exc::raise_with_message(OVERFLOW_ERROR, "date value out of range");
    };
    let (year, month, day) = civil_from_days(total);
    if !(MINYEAR..=MAXYEAR).contains(&year) {
        return exc::raise_with_message(OVERFLOW_ERROR, "date value out of range");
    }
    new_date(Date { year, month, day })
}

/// Shifts a `datetime` by a duration in microseconds.
fn datetime_shift(instance: i64, micros: i128) -> i64 {
    let Some(date) = date_from_instance(instance) else {
        return invalid_instance("invalid datetime object");
    };
    let Some(time) = time_from_instance(instance) else {
        return invalid_instance("invalid datetime object");
    };
    let base = i128::from(days_from_civil(date.year, date.month, date.day)) * MICROS_PER_DAY
        + i128::from(time.hour as i64) * 3_600 * i128::from(MICROS_PER_SECOND)
        + i128::from(time.minute as i64) * 60 * i128::from(MICROS_PER_SECOND)
        + i128::from(time.second as i64) * i128::from(MICROS_PER_SECOND)
        + i128::from(time.microsecond as i64);
    let total = match base.checked_add(micros) {
        Some(total) => total,
        None => return exc::raise_with_message(OVERFLOW_ERROR, "date value out of range"),
    };
    let parts = match parts_from_micros(total) {
        Ok(parts) => parts,
        Err(err) => return err,
    };
    new_datetime(
        Date {
            year: parts.year,
            month: parts.month,
            day: parts.day,
        },
        Time {
            hour: parts.hour,
            minute: parts.minute,
            second: parts.second,
            microsecond: parts.microsecond,
        },
        tzinfo_of(instance),
        fold_of(instance),
    )
}

/// Microseconds since the Unix epoch for a `datetime` instance, plus its
/// stored days for `date` instances.
fn instant_micros(instance: i64) -> Option<i128> {
    let date = date_from_instance(instance)?;
    let time = time_from_instance(instance)?;
    Some(
        i128::from(days_from_civil(date.year, date.month, date.day)) * MICROS_PER_DAY
            + i128::from(time.hour as i64) * 3_600 * i128::from(MICROS_PER_SECOND)
            + i128::from(time.minute as i64) * 60 * i128::from(MICROS_PER_SECOND)
            + i128::from(time.second as i64) * i128::from(MICROS_PER_SECOND)
            + i128::from(time.microsecond as i64),
    )
}

/// Adds `delta` to `instance` (`instance + delta`), or subtracts it when
/// `negate` is set.
fn shift(instance: i64, delta: Delta, negate: bool) -> i64 {
    match kind_of(instance) {
        // `date` arithmetic only sees the whole-day part of a duration, and the
        // sign is applied to that day count (`date - timedelta(hours=23)` keeps
        // the same day, because `timedelta(hours=23).days` is 0).
        Some(Kind::Date) => {
            let days = delta.days();
            date_shift(instance, if negate { -days } else { days })
        }
        Some(Kind::DateTime) => datetime_shift(instance, if negate { -delta.0 } else { delta.0 }),
        Some(Kind::Timedelta) => {
            let total = if negate { -delta.0 } else { delta.0 };
            match delta_from_instance(instance) {
                Some(receiver_delta) => delta_from_total(receiver_delta.0 + total)
                    .map(new_timedelta)
                    .unwrap_or_else(|err| err),
                None => invalid_instance("invalid timedelta object"),
            }
        }
        _ => value::NOT_IMPLEMENTED,
    }
}

fn add_method(args: &[i64]) -> i64 {
    let (Some(receiver), Some(other)) = (args.first().copied(), args.get(1).copied()) else {
        return arity_error("__add__", "exactly 2", args.len());
    };
    let Some(delta) = delta_from_instance(other) else {
        return value::NOT_IMPLEMENTED;
    };
    shift(receiver, delta, false)
}

/// Reflected addition: `timedelta + date`/`timedelta + datetime`.
///
/// Addition commutes, and the runtime calls the reflected dunder on the right
/// operand (`left + right` becomes `right.__radd__(left)`), so the duration
/// may arrive on either side: both `moment.__radd__(hour)` and
/// `hour.__radd__(moment)` mean `moment + hour`.
fn radd_method(args: &[i64]) -> i64 {
    let (Some(receiver), Some(other)) = (args.first().copied(), args.get(1).copied()) else {
        return arity_error("__radd__", "exactly 2", args.len());
    };
    if let Some(delta) = delta_from_instance(other) {
        return shift(receiver, delta, false);
    }
    match delta_from_instance(receiver) {
        Some(delta) => shift(other, delta, false),
        None => value::NOT_IMPLEMENTED,
    }
}

fn sub_method(args: &[i64]) -> i64 {
    let (Some(receiver), Some(other)) = (args.first().copied(), args.get(1).copied()) else {
        return arity_error("__sub__", "exactly 2", args.len());
    };
    match kind_of(receiver) {
        Some(Kind::Date) => {
            if let Some(delta) = delta_from_instance(other) {
                return shift(receiver, delta, true);
            }
            match date_from_instance(other) {
                // `date - datetime` is a type error in CPython; only a `date`
                // right operand produces a duration.
                Some(right) if kind_of(other) == Some(Kind::Date) => {
                    let Some(left) = date_from_instance(receiver) else {
                        return invalid_instance("invalid date object");
                    };
                    let days = days_from_civil(left.year, left.month, left.day)
                        - days_from_civil(right.year, right.month, right.day);
                    new_timedelta(Delta(i128::from(days) * MICROS_PER_DAY))
                }
                _ => value::NOT_IMPLEMENTED,
            }
        }
        Some(Kind::DateTime) => {
            if let Some(delta) = delta_from_instance(other) {
                return shift(receiver, delta, true);
            }
            if kind_of(other) == Some(Kind::DateTime) {
                let (Some(left), Some(right)) = (instant_micros(receiver), instant_micros(other))
                else {
                    return invalid_instance("invalid datetime object");
                };
                return match delta_from_total(left - right) {
                    Ok(delta) => new_timedelta(delta),
                    Err(err) => err,
                };
            }
            value::NOT_IMPLEMENTED
        }
        Some(Kind::Timedelta) => {
            let Some(right) = delta_from_instance(other) else {
                return value::NOT_IMPLEMENTED;
            };
            shift(receiver, right, true)
        }
        _ => value::NOT_IMPLEMENTED,
    }
}

// -------------------------------------------------------------------------
// Methods
// -------------------------------------------------------------------------

/// `datetime.now(tz=None)`/`datetime.utcnow()`: the current UTC wall clock.
fn now_method(args: &[i64]) -> i64 {
    if args.len() > 1 {
        return arity_error("now", "at most 1", args.len());
    }
    let tzinfo = match args.first().copied() {
        None => NONE,
        Some(candidate) => match tzinfo_argument(candidate) {
            Ok(tzinfo) => tzinfo,
            Err(err) => return err,
        },
    };
    let parts = now_parts();
    new_datetime(
        Date {
            year: parts.year,
            month: parts.month,
            day: parts.day,
        },
        Time {
            hour: parts.hour,
            minute: parts.minute,
            second: parts.second,
            microsecond: parts.microsecond,
        },
        tzinfo,
        0,
    )
}

fn utcnow_method(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return arity_error("utcnow", "exactly 0", args.len());
    }
    now_method(&[])
}

fn date_today_method(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return arity_error("today", "exactly 0", args.len());
    }
    let parts = now_parts();
    new_date(Date {
        year: parts.year,
        month: parts.month,
        day: parts.day,
    })
}

fn today_method(args: &[i64]) -> i64 {
    if !args.is_empty() {
        return arity_error("today", "exactly 0", args.len());
    }
    let parts = now_parts();
    new_datetime(
        Date {
            year: parts.year,
            month: parts.month,
            day: parts.day,
        },
        Time {
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
        },
        NONE,
        0,
    )
}

fn fromtimestamp_method(args: &[i64]) -> i64 {
    let Some(candidate) = args.first().copied() else {
        return arity_error("fromtimestamp", "1 to 2", 0);
    };
    if args.len() > 2 {
        return arity_error("fromtimestamp", "1 to 2", args.len());
    }
    let tzinfo = match args.get(1).copied() {
        None => NONE,
        Some(candidate) => match tzinfo_argument(candidate) {
            Ok(tzinfo) => tzinfo,
            Err(err) => return err,
        },
    };
    let micros = match timestamp_micros(candidate) {
        Ok(micros) => micros,
        Err(err) => return err,
    };
    let parts = match parts_from_micros(micros) {
        Ok(parts) => parts,
        Err(err) => return err,
    };
    new_datetime(
        Date {
            year: parts.year,
            month: parts.month,
            day: parts.day,
        },
        Time {
            hour: parts.hour,
            minute: parts.minute,
            second: parts.second,
            microsecond: parts.microsecond,
        },
        tzinfo,
        0,
    )
}

fn combine_method(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return arity_error("combine", "exactly 2", args.len());
    }
    let Some(date) = date_from_instance(args[0]).filter(|_| kind_of(args[0]) == Some(Kind::Date))
    else {
        return type_error(&format!(
            "combine() argument 1 must be datetime.date, not {}",
            value::type_name_of(args[0])
        ));
    };
    if kind_of(args[1]) != Some(Kind::Time) {
        return type_error(&format!(
            "combine() argument 2 must be datetime.time, not {}",
            value::type_name_of(args[1])
        ));
    }
    let Some(time) = time_from_instance(args[1]) else {
        return invalid_instance("invalid time object");
    };
    new_datetime(date, time, tzinfo_of(args[1]), fold_of(args[1]))
}

fn strftime_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("strftime", "exactly 2", args.len());
    };
    let Some(pattern) = args.get(1).copied() else {
        return arity_error("strftime", "exactly 2", args.len());
    };
    if args.len() > 2 {
        return arity_error("strftime", "exactly 2", args.len());
    }
    let Some(pattern) = value::as_str(pattern) else {
        return type_error(&format!(
            "strftime() argument 'format' must be str, not '{}'",
            value::type_name(pattern)
        ));
    };
    let Some(parts) = parts_of(receiver) else {
        return type_error("strftime() receiver is not a date, time or datetime object");
    };
    // Copy the pattern out before allocating the result: the borrowed string
    // must not be read after an allocation that may collect.
    let pattern = pattern.to_string();
    value::str_alloc(format(&parts, &pattern))
}

fn isoformat_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("isoformat", "at least 1", args.len());
    };
    let Some(kind) = kind_of(receiver) else {
        return type_error("isoformat() receiver is not a date, time or datetime object");
    };
    match kind {
        Kind::Date => {
            let Some(date) = date_from_instance(receiver) else {
                return invalid_instance("invalid date object");
            };
            value::str_alloc(date_text(date))
        }
        Kind::DateTime => {
            let Some(date) = date_from_instance(receiver) else {
                return invalid_instance("invalid datetime object");
            };
            let Some(time) = time_from_instance(receiver) else {
                return invalid_instance("invalid datetime object");
            };
            let separator = match args.get(1).copied() {
                None => 'T',
                Some(candidate) => match value::as_str(candidate) {
                    Some(text) => match text.chars().next() {
                        Some(character) => character,
                        None => {
                            return type_error("isoformat() separator must be a single character")
                        }
                    },
                    None => {
                        return type_error(&format!(
                            "isoformat() separator must be str, not '{}'",
                            value::type_name(candidate)
                        ))
                    }
                },
            };
            let offset = match offset_of(receiver) {
                Ok(offset) => offset,
                Err(err) => return err,
            };
            value::str_alloc(format!(
                "{}{separator}{}",
                date_text(date),
                time_text(time, offset)
            ))
        }
        Kind::Time => {
            let Some(time) = time_from_instance(receiver) else {
                return invalid_instance("invalid time object");
            };
            let offset = match offset_of(receiver) {
                Ok(offset) => offset,
                Err(err) => return err,
            };
            value::str_alloc(time_text(time, offset))
        }
        _ => type_error("isoformat() receiver is not a date, time or datetime object"),
    }
}

fn date_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("date", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("date", "exactly 1", args.len());
    }
    if kind_of(receiver) != Some(Kind::DateTime) {
        return type_error("date() requires a datetime object");
    }
    match date_from_instance(receiver) {
        Some(date) => new_date(date),
        None => invalid_instance("invalid datetime object"),
    }
}

fn time_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("time", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("time", "exactly 1", args.len());
    }
    if kind_of(receiver) != Some(Kind::DateTime) {
        return type_error("time() requires a datetime object");
    }
    match time_from_instance(receiver) {
        Some(time) => new_time(time, NONE, fold_of(receiver)),
        None => invalid_instance("invalid datetime object"),
    }
}

fn timestamp_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("timestamp", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("timestamp", "exactly 1", args.len());
    }
    let Some(parts) = parts_of(receiver) else {
        return type_error("timestamp() requires a datetime object");
    };
    let mut seconds = unix_seconds(&parts) as f64 + f64::from(parts.microsecond) / 1_000_000.0;
    match offset_of(receiver) {
        Ok(Some(offset)) => seconds -= offset as f64 / 1_000_000.0,
        Ok(None) => {}
        Err(err) => return err,
    }
    value::float_new(seconds)
}

/// The calendar date of a `date`/`datetime` receiver.
fn receiver_date(receiver: i64) -> Result<Date, i64> {
    match date_from_instance(receiver) {
        Some(date) => Ok(date),
        None => Err(invalid_instance("invalid date object")),
    }
}

fn weekday_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("weekday", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("weekday", "exactly 1", args.len());
    }
    match receiver_date(receiver) {
        Ok(date) => encode(i64::from(iso_weekday(date.year, date.month, date.day)) - 1),
        Err(err) => err,
    }
}

fn isoweekday_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("isoweekday", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("isoweekday", "exactly 1", args.len());
    }
    match receiver_date(receiver) {
        Ok(date) => encode(i64::from(iso_weekday(date.year, date.month, date.day))),
        Err(err) => err,
    }
}

fn toordinal_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("toordinal", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("toordinal", "exactly 1", args.len());
    }
    match receiver_date(receiver) {
        // CPython ordinals count days since 0001-01-01, not since the epoch;
        // the Unix epoch is day 1 of year 1 plus [`ORDINAL_EPOCH_OFFSET`] days.
        Ok(date) => {
            encode(days_from_civil(date.year, date.month, date.day) + ORDINAL_EPOCH_OFFSET + 1)
        }
        Err(err) => err,
    }
}

fn total_seconds_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("total_seconds", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("total_seconds", "exactly 1", args.len());
    }
    match delta_from_instance(receiver) {
        Some(delta) => value::float_new(delta.total_seconds()),
        None => invalid_instance("invalid timedelta object"),
    }
}

/// Applies the keyword fields of one `replace()` call.
fn apply_replacements(fields: &mut [(&str, i64)], keywords: &[(String, i64)]) -> Result<(), i64> {
    for (name, candidate) in keywords {
        let Some(slot) = fields.iter_mut().find(|(field, _)| *field == name.as_str()) else {
            return Err(type_error(&format!(
                "replace() got an unexpected keyword argument '{name}'"
            )));
        };
        slot.1 = *candidate;
    }
    Ok(())
}

fn replace_method(args: &[i64], keywords: &[(String, i64)]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("replace", "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error("replace", "exactly 1", args.len());
    }
    match kind_of(receiver) {
        Some(Kind::Date) => {
            let Some(date) = date_from_instance(receiver) else {
                return invalid_instance("invalid date object");
            };
            let mut fields = [
                ("year", encode(date.year)),
                ("month", encode(i64::from(date.month))),
                ("day", encode(i64::from(date.day))),
            ];
            if let Err(err) = apply_replacements(&mut fields, keywords) {
                return err;
            }
            let year = match integer_argument(fields[0].1) {
                Ok(year) => year,
                Err(err) => return err,
            };
            let month = match integer_argument(fields[1].1) {
                Ok(month) => month,
                Err(err) => return err,
            };
            let day = match integer_argument(fields[2].1) {
                Ok(day) => day,
                Err(err) => return err,
            };
            match validate_date(year, month, day) {
                Ok(date) => new_date(date),
                Err(err) => err,
            }
        }
        Some(Kind::Time) => replace_time(receiver, keywords),
        Some(Kind::DateTime) => replace_datetime(receiver, keywords),
        _ => type_error("replace() requires a date, time or datetime object"),
    }
}

fn replace_time(receiver: i64, keywords: &[(String, i64)]) -> i64 {
    let Some(time) = time_from_instance(receiver) else {
        return invalid_instance("invalid time object");
    };
    let mut fields: Vec<(&str, i64)> = vec![
        ("hour", encode(i64::from(time.hour))),
        ("minute", encode(i64::from(time.minute))),
        ("second", encode(i64::from(time.second))),
        ("microsecond", encode(i64::from(time.microsecond))),
        ("tzinfo", tzinfo_of(receiver)),
        ("fold", encode(fold_of(receiver))),
    ];
    if let Err(err) = apply_replacements(&mut fields, keywords) {
        return err;
    }
    let mut numbers = [0i64; 4];
    for (index, slot) in numbers.iter_mut().enumerate() {
        *slot = match integer_argument(fields[index].1) {
            Ok(number) => number,
            Err(err) => return err,
        };
    }
    let tzinfo = match tzinfo_argument(fields[4].1) {
        Ok(tzinfo) => tzinfo,
        Err(err) => return err,
    };
    let fold = match integer_argument(fields[5].1) {
        Ok(fold) if (0..=1).contains(&fold) => fold,
        Ok(_) => return invalid_instance("fold must be either 0 or 1"),
        Err(err) => return err,
    };
    match validate_time(numbers[0], numbers[1], numbers[2], numbers[3]) {
        Ok(time) => new_time(time, tzinfo, fold),
        Err(err) => err,
    }
}

fn replace_datetime(receiver: i64, keywords: &[(String, i64)]) -> i64 {
    let Some(date) = date_from_instance(receiver) else {
        return invalid_instance("invalid datetime object");
    };
    let Some(time) = time_from_instance(receiver) else {
        return invalid_instance("invalid datetime object");
    };
    let mut fields: Vec<(&str, i64)> = vec![
        ("year", encode(date.year)),
        ("month", encode(i64::from(date.month))),
        ("day", encode(i64::from(date.day))),
        ("hour", encode(i64::from(time.hour))),
        ("minute", encode(i64::from(time.minute))),
        ("second", encode(i64::from(time.second))),
        ("microsecond", encode(i64::from(time.microsecond))),
        ("tzinfo", tzinfo_of(receiver)),
        ("fold", encode(fold_of(receiver))),
    ];
    if let Err(err) = apply_replacements(&mut fields, keywords) {
        return err;
    }
    let mut numbers = [0i64; 7];
    for (index, slot) in numbers.iter_mut().enumerate() {
        *slot = match integer_argument(fields[index].1) {
            Ok(number) => number,
            Err(err) => return err,
        };
    }
    let tzinfo = match tzinfo_argument(fields[7].1) {
        Ok(tzinfo) => tzinfo,
        Err(err) => return err,
    };
    let fold = match integer_argument(fields[8].1) {
        Ok(fold) if (0..=1).contains(&fold) => fold,
        Ok(_) => return invalid_instance("fold must be either 0 or 1"),
        Err(err) => return err,
    };
    let date = match validate_date(numbers[0], numbers[1], numbers[2]) {
        Ok(date) => date,
        Err(err) => return err,
    };
    let time = match validate_time(numbers[3], numbers[4], numbers[5], numbers[6]) {
        Ok(time) => time,
        Err(err) => return err,
    };
    new_datetime(date, time, tzinfo, fold)
}

/// `dt.utcoffset()`/`dt.dst()`/`dt.tzname()`: delegates to the stored zone.
fn zone_method(args: &[i64], method: &str) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error(method, "exactly 1", args.len());
    };
    if args.len() > 1 {
        return arity_error(method, "exactly 1", args.len());
    }
    let tzinfo = tzinfo_of(receiver);
    if tzinfo == NONE {
        return NONE;
    }
    let name = method.as_bytes();
    let args = [receiver];
    crate::methods::call(tzinfo, name.as_ptr(), name.len(), 1, args.as_ptr())
}

/// `tzinfo`'s base methods report an unknown zone.
fn unknown_zone_method(args: &[i64]) -> i64 {
    if args.is_empty() {
        return arity_error("tzinfo method", "at least 1", 0);
    }
    NONE
}

fn repr_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("__repr__", "exactly 1", args.len());
    };
    match render(receiver, true) {
        Some(text) => value::str_alloc(text),
        None => type_error("__repr__() receiver is not a datetime object"),
    }
}

fn str_method(args: &[i64]) -> i64 {
    let Some(receiver) = args.first().copied() else {
        return arity_error("__str__", "exactly 1", args.len());
    };
    match render(receiver, false) {
        Some(text) => value::str_alloc(text),
        None => type_error("__str__() receiver is not a datetime object"),
    }
}

// -------------------------------------------------------------------------
// Module dispatch
// -------------------------------------------------------------------------

/// Dispatches a native `datetime` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        NOW_METHOD => now_method(args),
        UTCNOW_METHOD => utcnow_method(args),
        DATE_TODAY_METHOD => date_today_method(args),
        TODAY_METHOD => today_method(args),
        FROMTIMESTAMP_METHOD => fromtimestamp_method(args),
        COMBINE_METHOD => combine_method(args),
        STRFTIME_METHOD => strftime_method(args),
        ISOFORMAT_METHOD => isoformat_method(args),
        DATE_METHOD => date_method(args),
        TIME_METHOD => time_method(args),
        TIMESTAMP_METHOD => timestamp_method(args),
        WEEKDAY_METHOD => weekday_method(args),
        ISOWEEKDAY_METHOD => isoweekday_method(args),
        TOORDINAL_METHOD => toordinal_method(args),
        TOTAL_SECONDS_METHOD => total_seconds_method(args),
        UTCOFFSET_METHOD => zone_method(args, "utcoffset"),
        DST_METHOD => zone_method(args, "dst"),
        TZNAME_METHOD => zone_method(args, "tzname"),
        TZINFO_UTCOFFSET_METHOD | TZINFO_DST_METHOD | TZINFO_TZNAME_METHOD => {
            unknown_zone_method(args)
        }
        REPR_METHOD => repr_method(args),
        STR_METHOD => str_method(args),
        EQ_METHOD => compare_method(args, OP_EQ),
        LT_METHOD => compare_method(args, OP_LT),
        LE_METHOD => compare_method(args, OP_LE),
        GT_METHOD => compare_method(args, OP_GT),
        GE_METHOD => compare_method(args, OP_GE),
        ADD_METHOD => add_method(args),
        RADDMETHOD => radd_method(args),
        SUB_METHOD => sub_method(args),
        REPLACE_METHOD => replace_method(args, &[]),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'datetime' has no attribute '{function}'"),
        ),
    }
}

/// Keyword dispatch for the native methods that declare dynamic keywords
/// (`replace(year=..., tzinfo=...)`).
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    match function {
        REPLACE_METHOD => replace_method(args, keywords),
        _ => call(function, args),
    }
}

/// Module constants. The value classes are cached, so `type()`/`isinstance()`
/// see the same class object on every lookup.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "date" => date_class(),
        "time" => time_class(),
        "datetime" => datetime_class(),
        "timedelta" => timedelta_class(),
        "tzinfo" => tzinfo_class(),
        "MINYEAR" => encode(MINYEAR),
        "MAXYEAR" => encode(MAXYEAR),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'datetime' has no attribute '{name}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> i64 {
        value::encode_int(n).expect("representable")
    }

    fn text(raw: &str) -> i64 {
        value::str_alloc(raw.to_string())
    }

    fn date_instance(year: i64, month: i64, day: i64) -> i64 {
        instantiate(date_class(), &[int(year), int(month), int(day)]).expect("date class")
    }

    fn time_instance(hour: i64, minute: i64, second: i64, microsecond: i64) -> i64 {
        instantiate(
            time_class(),
            &[int(hour), int(minute), int(second), int(microsecond)],
        )
        .expect("time class")
    }

    fn datetime_instance(
        year: i64,
        month: i64,
        day: i64,
        hour: i64,
        minute: i64,
        second: i64,
        microsecond: i64,
    ) -> i64 {
        instantiate(
            datetime_class(),
            &[
                int(year),
                int(month),
                int(day),
                int(hour),
                int(minute),
                int(second),
                int(microsecond),
            ],
        )
        .expect("datetime class")
    }

    fn delta_instance(days: i64) -> i64 {
        instantiate(timedelta_class(), &[int(days)]).expect("timedelta class")
    }

    fn as_int(v: i64) -> i64 {
        value::as_integer(v).expect("integer")
    }

    fn as_text(v: i64) -> String {
        value::as_str(v).expect("string").to_string()
    }

    fn sample() -> Parts {
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
        let parts = sample();
        assert_eq!(format(&parts, "%Y-%m-%d %H:%M:%S"), "2023-11-14 22:13:20");
        assert_eq!(format(&parts, "%Y%m%d-%H%M%S"), "20231114-221320");
        assert_eq!(format(&parts, "%f"), "123456");
        assert_eq!(format(&parts, "%a %A"), "Tue Tuesday");
        assert_eq!(format(&parts, "%b %B"), "Nov November");
        assert_eq!(format(&parts, "%j"), "318");
        assert_eq!(format(&parts, "%I %p"), "10 PM");
        assert_eq!(format(&parts, "%z %Z"), "+0000 UTC");
        assert_eq!(format(&parts, "%s"), "1700000000");
        assert_eq!(format(&parts, "%V %G"), "46 2023");
        assert_eq!(format(&parts, "%U %W"), "46 46");
        assert_eq!(format(&parts, "%c"), "Tue Nov 14 22:13:20 2023");
        assert_eq!(format(&parts, "%F %T"), "2023-11-14 22:13:20");
        assert_eq!(format(&parts, "%%n"), "%n");
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
        let parts = from_unix(-1, 500_000);
        assert_eq!(
            format(&parts, "%Y-%m-%d %H:%M:%S.%f"),
            "1969-12-31 23:59:59.500000"
        );
        let (seconds, micros) = unix_parts(UNIX_EPOCH - std::time::Duration::new(1, 500_000_000));
        assert_eq!((seconds, micros), (-2, 500_000));
    }

    #[test]
    fn module_exposes_cached_value_classes() {
        exc::init();
        for (name, expected) in [
            ("date", DATE_CLASS),
            ("time", TIME_CLASS),
            ("datetime", DATETIME_CLASS),
            ("timedelta", TIMEDELTA_CLASS),
            ("tzinfo", TZINFO_CLASS),
        ] {
            let class = constant(name);
            assert!(!exc::pending(), "{name} raised");
            assert_eq!(value::class_display_name(class), Some(expected));
            assert_eq!(constant(name), class, "{name} is cached");
        }
        assert_eq!(as_int(constant("MINYEAR")), 1);
        assert_eq!(as_int(constant("MAXYEAR")), 9_999);
        // `datetime` is a date subclass, like CPython.
        assert_ne!(
            crate::builtins::issubclass(constant("datetime"), constant("date")),
            value::FALSE
        );
    }

    #[test]
    fn date_constructor_validates_and_exposes_attributes() {
        exc::init();
        let date = date_instance(2024, 2, 29);
        assert!(!exc::pending());
        assert_eq!(component(date, "year"), Some(2024));
        assert_eq!(component(date, "month"), Some(2));
        assert_eq!(component(date, "day"), Some(29));
        assert_eq!(as_text(str_method(&[date])), "2024-02-29");
        assert_eq!(as_text(repr_method(&[date])), "datetime.date(2024, 2, 29)");

        // Range validation matches CPython.
        for (year, month, day, expected) in [
            (0, 1, 1, "year 0 is out of range"),
            (10_000, 1, 1, "year 10000 is out of range"),
            (2023, 0, 1, "month must be in 1..12"),
            (2023, 13, 1, "month must be in 1..12"),
            (2023, 2, 29, "day is out of range for month"),
            (2023, 4, 31, "day is out of range for month"),
        ] {
            exc::init();
            instantiate(date_class(), &[int(year), int(month), int(day)]);
            assert_eq!(
                exc::pending_tag(),
                i64::from(VALUE_ERROR),
                "{year}-{month}-{day}"
            );
            assert_eq!(value::to_string(exc::pending_value()), expected);
        }

        exc::init();
        instantiate(date_class(), &[int(2024), int(1)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();
        instantiate(date_class(), &[text("2024"), int(1), int(1)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }

    #[test]
    fn time_and_datetime_attributes_and_repr() {
        exc::init();
        let time = time_instance(13, 5, 9, 250);
        assert_eq!(component(time, "hour"), Some(13));
        assert_eq!(component(time, "minute"), Some(5));
        assert_eq!(component(time, "second"), Some(9));
        assert_eq!(component(time, "microsecond"), Some(250));
        assert_eq!(tzinfo_of(time), NONE);
        assert_eq!(as_text(str_method(&[time])), "13:05:09.000250");
        assert_eq!(
            as_text(repr_method(&[time])),
            "datetime.time(13, 5, 9, 250)"
        );

        let moment = datetime_instance(2024, 2, 29, 13, 5, 9, 250);
        assert_eq!(as_text(str_method(&[moment])), "2024-02-29 13:05:09.000250");
        assert_eq!(
            as_text(repr_method(&[moment])),
            "datetime.datetime(2024, 2, 29, 13, 5, 9, 250)"
        );
        // Midnight keeps the mandatory hour/minute fields only.
        let midnight = datetime_instance(2024, 1, 1, 0, 0, 0, 0);
        assert_eq!(
            as_text(repr_method(&[midnight])),
            "datetime.datetime(2024, 1, 1, 0, 0)"
        );

        exc::init();
        instantiate(time_class(), &[int(24)]);
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        exc::init();
        instantiate(
            datetime_class(),
            &[int(2024), int(1), int(1), int(0), int(60)],
        );
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
    }

    #[test]
    fn timedelta_normalizes_its_components() {
        exc::init();
        let delta = instantiate(timedelta_class(), &[int(1), int(3600)]).expect("timedelta");
        assert!(!exc::pending());
        assert_eq!(component(delta, "days"), Some(1));
        assert_eq!(component(delta, "seconds"), Some(3_600));
        assert_eq!(component(delta, "microseconds"), Some(0));
        assert_eq!(as_text(str_method(&[delta])), "1 day, 1:00:00");
        assert_eq!(
            as_text(repr_method(&[delta])),
            "datetime.timedelta(days=1, seconds=3600)"
        );

        let negative = instantiate(timedelta_class(), &[int(-1)]).expect("timedelta");
        assert_eq!(component(negative, "days"), Some(-1));
        assert_eq!(component(negative, "seconds"), Some(0));
        assert_eq!(as_text(str_method(&[negative])), "-1 day, 0:00:00");

        let normalized = instantiate(
            timedelta_class(),
            &[int(0), int(0), int(0), int(0), int(0), int(25)],
        )
        .expect("timedelta");
        assert_eq!(component(normalized, "days"), Some(1));
        assert_eq!(component(normalized, "seconds"), Some(3_600));

        let seconds =
            instantiate(timedelta_class(), &[int(0), int(90), int(500_000)]).expect("timedelta");
        assert_eq!(as_text(str_method(&[seconds])), "0:01:30.500000");
        let total = call(TOTAL_SECONDS_METHOD, &[seconds]);
        assert_eq!(value::as_float(total), Some(90.5));

        let empty = instantiate(timedelta_class(), &[]).expect("timedelta");
        assert_eq!(as_text(repr_method(&[empty])), "datetime.timedelta(0)");

        exc::init();
        instantiate(timedelta_class(), &[int(1_000_000_000)]);
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));
        exc::init();
        instantiate(timedelta_class(), &[text("1")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }

    #[test]
    fn keyword_constructors_bind_declared_parameters() {
        exc::init();
        let delta = instantiate_kw(
            timedelta_class(),
            &[],
            &[("days".to_string(), int(2)), ("hours".to_string(), int(3))],
        )
        .expect("timedelta");
        assert!(!exc::pending());
        assert_eq!(component(delta, "days"), Some(2));
        assert_eq!(component(delta, "seconds"), Some(10_800));

        let moment = instantiate_kw(
            datetime_class(),
            &[],
            &[
                ("year".to_string(), int(2024)),
                ("month".to_string(), int(7)),
                ("day".to_string(), int(4)),
                ("fold".to_string(), int(1)),
            ],
        )
        .expect("datetime");
        assert!(!exc::pending());
        assert_eq!(component(moment, "year"), Some(2024));
        assert_eq!(component(moment, "fold"), Some(1));

        exc::init();
        instantiate_kw(
            timedelta_class(),
            &[int(1)],
            &[("days".to_string(), int(2))],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        instantiate_kw(
            timedelta_class(),
            &[],
            &[("fortnights".to_string(), int(2))],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }

    #[test]
    fn equality_and_ordering_compare_components() {
        exc::init();
        let first = date_instance(2024, 2, 29);
        let same = date_instance(2024, 2, 29);
        let later = date_instance(2024, 3, 1);
        assert_eq!(call(EQ_METHOD, &[first, same]), value::TRUE);
        assert_eq!(call(EQ_METHOD, &[first, later]), value::FALSE);
        assert_eq!(call(LT_METHOD, &[first, later]), value::TRUE);
        assert_eq!(call(LE_METHOD, &[first, same]), value::TRUE);
        assert_eq!(call(GT_METHOD, &[later, first]), value::TRUE);
        assert_eq!(call(GE_METHOD, &[first, later]), value::FALSE);

        // Different classes decline the comparison.
        assert_eq!(
            call(
                EQ_METHOD,
                &[first, datetime_instance(2024, 2, 29, 0, 0, 0, 0)]
            ),
            value::NOT_IMPLEMENTED
        );
        assert_eq!(
            call(EQ_METHOD, &[time_instance(1, 2, 3, 4), first]),
            value::NOT_IMPLEMENTED
        );
        let one_hour = instantiate(timedelta_class(), &[int(0), int(3_600)]).expect("timedelta");
        let two_hours = instantiate(timedelta_class(), &[int(0), int(7_200)]).expect("timedelta");
        assert_eq!(call(LT_METHOD, &[one_hour, two_hours]), value::TRUE);
    }

    #[test]
    fn dates_and_datetimes_support_timedelta_arithmetic() {
        exc::init();
        let day = delta_instance(1);
        let date = date_instance(2024, 2, 28);
        let next = call(ADD_METHOD, &[date, day]);
        assert_eq!(value::instance_class_name(next), Some("date"));
        assert_eq!(as_text(call(STR_METHOD, &[next])), "2024-02-29");
        let back = call(SUB_METHOD, &[next, day]);
        assert_eq!(value::instance_class_name(back), Some("date"));
        assert_eq!(as_text(call(STR_METHOD, &[back])), "2024-02-28");

        let span = call(SUB_METHOD, &[next, date]);
        assert_eq!(value::instance_class_name(span), Some("timedelta"));
        assert_eq!(as_text(call(STR_METHOD, &[span])), "1 day, 0:00:00");

        let moment = datetime_instance(2024, 2, 28, 23, 30, 0, 0);
        let later = call(ADD_METHOD, &[moment, day]);
        assert_eq!(value::instance_class_name(later), Some("datetime"));
        assert_eq!(as_text(call(STR_METHOD, &[later])), "2024-02-29 23:30:00");
        let difference = call(SUB_METHOD, &[later, moment]);
        assert_eq!(as_text(call(STR_METHOD, &[difference])), "1 day, 0:00:00");

        let hour = instantiate(timedelta_class(), &[int(0), int(3_600)]).expect("timedelta");
        let shifted = call(ADD_METHOD, &[moment, hour]);
        assert_eq!(as_text(call(STR_METHOD, &[shifted])), "2024-02-29 00:30:00");
        // The reflected form covers `timedelta + datetime`. The runtime calls
        // `right.__radd__(left)` for `left + right`, and addition commutes, so
        // the duration may arrive on either side.
        let reflected = call(RADDMETHOD, &[hour, moment]);
        assert_eq!(
            as_text(call(STR_METHOD, &[reflected])),
            "2024-02-29 00:30:00"
        );
        let dispatched = call(RADDMETHOD, &[moment, hour]);
        assert_eq!(
            as_text(call(STR_METHOD, &[dispatched])),
            "2024-02-29 00:30:00"
        );

        // Negative durations normalize like CPython: the day count carries the
        // sign and `seconds`/`microseconds` stay in their non-negative ranges,
        // so the result renders and stays usable as a `timedelta`.
        let backwards = call(SUB_METHOD, &[date, next]);
        assert_eq!(value::instance_class_name(backwards), Some("timedelta"));
        assert_eq!(as_text(call(STR_METHOD, &[backwards])), "-1 day, 0:00:00");
        let mixed = call(ADD_METHOD, &[backwards, hour]);
        assert_eq!(as_text(call(STR_METHOD, &[mixed])), "-1 day, 1:00:00");

        // `timedelta ± timedelta` stays a `timedelta`.
        let doubled = call(ADD_METHOD, &[hour, hour]);
        assert_eq!(value::instance_class_name(doubled), Some("timedelta"));
        assert_eq!(as_text(call(STR_METHOD, &[doubled])), "2:00:00");
        let shrunk = call(SUB_METHOD, &[hour, day]);
        assert_eq!(as_text(call(STR_METHOD, &[shrunk])), "-1 day, 1:00:00");

        // Out-of-range results report OverflowError.
        exc::init();
        let last = date_instance(9999, 12, 31);
        call(ADD_METHOD, &[last, day]);
        assert_eq!(exc::pending_tag(), i64::from(OVERFLOW_ERROR));

        // Unsupported operands decline instead of raising.
        assert_eq!(call(ADD_METHOD, &[date, date]), value::NOT_IMPLEMENTED);
        assert_eq!(
            call(
                SUB_METHOD,
                &[date, datetime_instance(2024, 2, 28, 0, 0, 0, 0)]
            ),
            value::NOT_IMPLEMENTED
        );
    }

    #[test]
    fn strftime_reads_components_from_instances() {
        exc::init();
        let moment = datetime_instance(2023, 11, 14, 22, 13, 20, 123_456);
        let rendered = call(STRFTIME_METHOD, &[moment, text("%Y-%m-%dT%H:%M:%S.%f")]);
        assert!(!exc::pending());
        assert_eq!(as_text(rendered), "2023-11-14T22:13:20.123456");

        let date = date_instance(2023, 11, 14);
        assert_eq!(
            as_text(call(STRFTIME_METHOD, &[date, text("%d/%m/%Y %H:%M")])),
            "14/11/2023 00:00"
        );

        let time = time_instance(7, 8, 9, 0);
        assert_eq!(
            as_text(call(STRFTIME_METHOD, &[time, text("%H:%M:%S")])),
            "07:08:09"
        );

        assert_eq!(
            as_text(call(ISOFORMAT_METHOD, &[moment])),
            "2023-11-14T22:13:20.123456"
        );
        assert_eq!(
            as_text(call(ISOFORMAT_METHOD, &[moment, text(" ")])),
            "2023-11-14 22:13:20.123456"
        );
        assert_eq!(as_text(call(ISOFORMAT_METHOD, &[date])), "2023-11-14");
        assert_eq!(as_text(call(ISOFORMAT_METHOD, &[time])), "07:08:09");
    }

    #[test]
    fn calendar_helpers_match_cpython() {
        exc::init();
        let date = date_instance(2023, 11, 14);
        assert_eq!(as_int(call(WEEKDAY_METHOD, &[date])), 1);
        assert_eq!(as_int(call(ISOWEEKDAY_METHOD, &[date])), 2);
        assert_eq!(as_int(call(TOORDINAL_METHOD, &[date])), 738_838);
        // Ordinals count days since 0001-01-01, not since the Unix epoch.
        assert_eq!(
            as_int(call(TOORDINAL_METHOD, &[date_instance(1970, 1, 1)])),
            719_163
        );
        assert_eq!(as_int(call(TOORDINAL_METHOD, &[date_instance(1, 1, 1)])), 1);

        let moment = datetime_instance(2023, 11, 14, 22, 13, 20, 0);
        assert_eq!(as_int(call(WEEKDAY_METHOD, &[moment])), 1);
        let stamp = call(TIMESTAMP_METHOD, &[moment]);
        assert_eq!(value::as_float(stamp), Some(1_700_000_000.0));

        let rebuilt = call(FROMTIMESTAMP_METHOD, &[stamp]);
        assert_eq!(as_text(call(STR_METHOD, &[rebuilt])), "2023-11-14 22:13:20");

        let date_part = call(DATE_METHOD, &[moment]);
        assert_eq!(as_text(call(STR_METHOD, &[date_part])), "2023-11-14");
        let time_part = call(TIME_METHOD, &[moment]);
        assert_eq!(as_text(call(STR_METHOD, &[time_part])), "22:13:20");

        let combined = call(COMBINE_METHOD, &[date, time_part]);
        assert_eq!(
            as_text(call(STR_METHOD, &[combined])),
            "2023-11-14 22:13:20"
        );
    }

    #[test]
    fn replace_updates_only_known_fields() {
        exc::init();
        let date = date_instance(2024, 2, 29);
        let _ = replace_method(&[date], &[("year".to_string(), int(2023))]);
        // 2023 is not a leap year, so the day must be replaced as well.
        assert_eq!(exc::pending_tag(), i64::from(VALUE_ERROR));
        exc::init();
        let replaced = replace_method(
            &[date],
            &[
                ("year".to_string(), int(2023)),
                ("day".to_string(), int(28)),
            ],
        );
        assert!(
            !exc::pending(),
            "{}",
            value::to_string(exc::pending_value())
        );
        let rendered = call(STR_METHOD, &[replaced]);
        assert_eq!(
            as_text(rendered),
            "2023-02-28",
            "rendered type {}",
            value::type_name(rendered)
        );

        let moment = datetime_instance(2024, 2, 29, 1, 2, 3, 4);
        let replaced = replace_method(&[moment], &[("month".to_string(), int(3))]);
        assert_eq!(
            as_text(call(STR_METHOD, &[replaced])),
            "2024-03-29 01:02:03.000004"
        );

        exc::init();
        replace_method(&[date], &[("hours".to_string(), int(1))]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        exc::init();

        let time = time_instance(1, 2, 3, 4);
        let replaced = replace_method(&[time], &[("second".to_string(), int(30))]);
        assert_eq!(as_text(call(STR_METHOD, &[replaced])), "01:02:30.000004");
    }

    #[test]
    fn now_returns_fresh_instances() {
        exc::init();
        let first = call(NOW_METHOD, &[]);
        let second = call(NOW_METHOD, &[]);
        assert!(!exc::pending());
        assert_eq!(kind_of(first), Some(Kind::DateTime));
        assert_ne!(first, second);
        let year = component(first, "year").expect("year");
        assert!(year >= 2020);
        assert!(value::instance_attr(first, "month").is_some());
        assert!(value::instance_attr(first, "microsecond").is_some());

        let rendered = call(STRFTIME_METHOD, &[first, text("%Y%m%d-%H%M%S")]);
        let rendered = as_text(rendered);
        assert_eq!(rendered.len(), 15);
        assert_eq!(&rendered[8..9], "-");
        assert!(rendered[..8].bytes().all(|byte| byte.is_ascii_digit()));

        let today = call(DATE_TODAY_METHOD, &[]);
        assert_eq!(kind_of(today), Some(Kind::Date));
    }

    #[test]
    fn invalid_arguments_report_errors() {
        exc::init();
        call(STRFTIME_METHOD, &[]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        let moment = datetime_instance(2023, 11, 14, 22, 13, 20, 0);
        call(STRFTIME_METHOD, &[moment]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        call(STRFTIME_METHOD, &[moment, int(1)]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));

        exc::init();
        call("missing", &[]);
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));

        exc::init();
        constant("missing");
        assert_eq!(exc::pending_tag(), i64::from(ATTRIBUTE_ERROR));

        exc::init();
        assert_eq!(call(UTCOFFSET_METHOD, &[moment]), NONE);
        assert!(!exc::pending());

        exc::init();
        instantiate_kw(
            date_class(),
            &[int(2024), int(1), int(1)],
            &[("year".to_string(), int(2024))],
        );
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }

    #[test]
    fn mutated_components_are_rejected() {
        exc::init();
        let moment = datetime_instance(2023, 11, 14, 22, 13, 20, 0);
        value::instance_setattr(
            moment,
            "month".to_string(),
            value::encode_int(13).expect("int"),
        );
        assert!(date_from_instance(moment).is_none());
        call(STRFTIME_METHOD, &[moment, text("%Y")]);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
    }
}
