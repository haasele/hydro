//! `zoneinfo`: IANA time zones backed by the system TZif database.
//!
//! `ZoneInfo(key)` reads the compiled zone file under `TZPATH`
//! (`/usr/share/zoneinfo` by default, `PYTHONTZPATH` overrides it) and parses
//! the TZif v1/v2/v3 format: transitions, UTC offsets, DST flags and
//! abbreviations. `available_timezones()` walks the same database and keeps
//! only files whose content is a TZif zone.
//!
//! # Return values
//!
//! Hydro has no `datetime.timedelta` type, so `utcoffset()` and `dst()` return
//! the offset in **seconds** (an `int`) instead of a `timedelta`. `tzname()`
//! returns the abbreviation string. `fromutc()` shifts the components of a
//! native `datetime` instance and returns a new instance of the same class.
//! A missing zone raises `ZoneInfoNotFoundError` (a `KeyError` subclass).
//!
//! The module-level `TZPATH` tuple is the search path in effect;
//! `reset_tzpath()` replaces it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use hydro_ir::MethodKind;

use crate::exc::{self, ATTRIBUTE_ERROR, KEY_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value::{self, NONE};

const MODULE: &str = "zoneinfo";

const INIT: &str = "_zoneinfo_init";
const GET_KEY: &str = "_zoneinfo_get_key";
const UTCOFFSET: &str = "_zoneinfo_utcoffset";
const DST: &str = "_zoneinfo_dst";
const TZNAME: &str = "_zoneinfo_tzname";
const FROMUTC: &str = "_zoneinfo_fromutc";
const STR: &str = "_zoneinfo_str";
const REPR: &str = "_zoneinfo_repr";

/// Default search path, matching CPython's `zoneinfo.TZPATH`.
const DEFAULT_TZPATH: [&str; 4] = [
    "/usr/share/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
];

/// Zone names that are files in the database but are not valid keys.
const INVALID_KEYS: [&str; 3] = ["localtime", "posixrules", "Factory"];

static TZPATH_OVERRIDE: Mutex<Option<Vec<String>>> = Mutex::new(None);
static ZONES: LazyLock<Mutex<HashMap<String, Arc<Tzif>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn zone_cache() -> MutexGuard<'static, HashMap<String, Arc<Tzif>>> {
    ZONES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn encode(n: i64) -> i64 {
    value::encode_int(n).unwrap_or_else(|| exc::raise_with_message(exc::OVERFLOW_ERROR, "overflow"))
}

fn type_error(message: &str) -> i64 {
    exc::raise_with_message(TYPE_ERROR, message)
}

/// Current search path: `reset_tzpath()` override, else `PYTHONTZPATH`, else
/// the CPython defaults.
fn tzpath() -> Vec<String> {
    if let Some(path) = TZPATH_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return path;
    }
    if let Ok(raw) = std::env::var("PYTHONTZPATH") {
        let entries: Vec<String> = raw
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect();
        if !entries.is_empty() {
            return entries;
        }
    }
    DEFAULT_TZPATH.iter().map(|path| path.to_string()).collect()
}

fn not_found(key: &str) -> i64 {
    let class = not_found_class();
    let args = [value::str_alloc(format!(
        "No time zone found with key {key}"
    ))];
    let instance = crate::builtins::call(class, args.len(), args.as_ptr());
    if exc::pending() {
        return instance;
    }
    exc::raise_value(instance)
}

/// `zoneinfo.ZoneInfoNotFoundError` (a `KeyError` subclass).
fn not_found_class() -> i64 {
    if let Some(class) = crate::core::globals::registry_get("zoneinfo.NotFound") {
        return class;
    }
    let base = exc::exception_class(KEY_ERROR);
    let class = value::class_new("ZoneInfoNotFoundError".to_string(), base, Vec::new());
    crate::core::globals::registry_set("zoneinfo.NotFound", class);
    class
}

// -------------------------------------------------------------------------
// TZif parsing
// -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct LocalTimeType {
    offset: i32,
    isdst: bool,
    abbr: String,
}

#[derive(Debug)]
struct Tzif {
    transitions: Vec<i64>,
    /// Index into `types` for each transition.
    indices: Vec<u8>,
    types: Vec<LocalTimeType>,
    /// First standard-time type, used for the DST adjustment.
    standard: i32,
}

static UTC_TYPE: LocalTimeType = LocalTimeType {
    offset: 0,
    isdst: false,
    abbr: String::new(),
};

impl Tzif {
    fn type_at(&self, timestamp: i64) -> &LocalTimeType {
        if self.transitions.is_empty() || self.types.is_empty() {
            return self.types.first().unwrap_or(&UTC_TYPE);
        }
        let position = match self.transitions.binary_search(&timestamp) {
            Ok(index) => index,
            Err(0) => return &self.types[0],
            Err(index) => index - 1,
        };
        let index = self
            .indices
            .get(position)
            .copied()
            .map(usize::from)
            .unwrap_or(0);
        self.types.get(index).unwrap_or(&self.types[0])
    }

    /// Offset chosen for a *local* wall time (two fixed-point passes).
    fn offset_at_local(&self, local_timestamp: i64) -> i64 {
        let mut offset = i64::from(self.type_at(local_timestamp).offset);
        for _ in 0..2 {
            offset = i64::from(self.type_at(local_timestamp.saturating_sub(offset)).offset);
        }
        offset
    }

    fn dst_at(&self, timestamp: i64) -> i64 {
        let kind = self.type_at(timestamp);
        if kind.isdst {
            i64::from(kind.offset) - i64::from(self.standard)
        } else {
            0
        }
    }
}

fn be_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn be_i32(data: &[u8], offset: usize) -> Option<i32> {
    be_u32(data, offset).map(|n| n as i32)
}

fn be_i64(data: &[u8], offset: usize) -> Option<i64> {
    let bytes = data.get(offset..offset.checked_add(8)?)?;
    Some(i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

struct Header {
    _isutcnt: usize,
    _isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
    version: u8,
}

fn parse_header(data: &[u8], offset: usize) -> Option<Header> {
    let magic = data.get(offset..offset.checked_add(4)?)?;
    if magic[0] != b'T' || magic[1] != b'Z' || magic[2] != b'i' || magic[3] != b'f' {
        return None;
    }
    let version = *data.get(offset + 4)?;
    let counts = offset + 20;
    let header = Header {
        _isutcnt: be_u32(data, counts)? as usize,
        _isstdcnt: be_u32(data, counts + 4)? as usize,
        leapcnt: be_u32(data, counts + 8)? as usize,
        timecnt: be_u32(data, counts + 12)? as usize,
        typecnt: be_u32(data, counts + 16)? as usize,
        charcnt: be_u32(data, counts + 20)? as usize,
        version,
    };
    if header.typecnt == 0 || header.charcnt == 0 || header.timecnt > 1_000_000 {
        return None;
    }
    Some(header)
}

/// Size in bytes of the data block described by `header`.
fn block_size(header: &Header, time_size: usize) -> usize {
    header
        .timecnt
        .saturating_mul(time_size + 1)
        .saturating_add(header.typecnt.saturating_mul(6))
        .saturating_add(header.charcnt)
        .saturating_add(header.leapcnt.saturating_mul(time_size + 4))
        .saturating_add(header._isstdcnt)
        .saturating_add(header._isutcnt)
}

fn parse_block(data: &[u8], offset: usize, header: &Header, time_size: usize) -> Option<Tzif> {
    let mut cursor = offset;
    let mut transitions = Vec::with_capacity(header.timecnt);
    for _ in 0..header.timecnt {
        let value = if time_size == 8 {
            be_i64(data, cursor)?
        } else {
            i64::from(be_i32(data, cursor)?)
        };
        transitions.push(value);
        cursor += time_size;
    }
    let mut indices = Vec::with_capacity(header.timecnt);
    for _ in 0..header.timecnt {
        let index = *data.get(cursor)?;
        indices.push(index);
        cursor += 1;
    }
    let type_block = cursor;
    cursor = cursor.checked_add(header.typecnt.checked_mul(6)?)?;
    let chars = data.get(cursor..cursor.checked_add(header.charcnt)?)?;
    let mut types = Vec::with_capacity(header.typecnt);
    let mut standard: Option<i32> = None;
    for index in 0..header.typecnt {
        let entry = type_block.checked_add(index.checked_mul(6)?)?;
        let offset = be_i32(data, entry)?;
        let isdst = *data.get(entry + 4)? != 0;
        let abbr_index = usize::from(*data.get(entry + 5)?);
        let tail = chars.get(abbr_index..)?;
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(tail.len());
        let abbr = String::from_utf8_lossy(&tail[..end]).into_owned();
        if !isdst && standard.is_none() {
            standard = Some(offset);
        }
        types.push(LocalTimeType {
            offset,
            isdst,
            abbr,
        });
    }
    Some(Tzif {
        transitions,
        indices,
        standard: standard.unwrap_or_else(|| types.first().map(|kind| kind.offset).unwrap_or(0)),
        types,
    })
}

/// Parses a TZif v1/v2/v3 file, preferring the 64-bit block.
fn parse_tzif(data: &[u8]) -> Option<Tzif> {
    let header = parse_header(data, 0)?;
    let v1_size = block_size(&header, 4);
    let v1_end = 44usize.checked_add(v1_size)?;
    if header.version >= b'2' {
        let second = parse_header(data, v1_end)?;
        let block = v1_end.checked_add(44)?;
        if let Some(zone) = parse_block(data, block, &second, 8) {
            return Some(zone);
        }
    }
    parse_block(data, 44, &header, 4)
}

/// Reads and parses one zone file.
fn load_zone(key: &str) -> Result<Arc<Tzif>, ()> {
    if !valid_key(key) {
        return Err(());
    }
    if let Some(zone) = zone_cache().get(key).cloned() {
        return Ok(zone);
    }
    for root in tzpath() {
        let path = Path::new(&root).join(key);
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let Some(zone) = parse_tzif(&data) else {
            return Err(());
        };
        let zone = Arc::new(zone);
        zone_cache().insert(key.to_string(), Arc::clone(&zone));
        return Ok(zone);
    }
    Err(())
}

fn valid_key(key: &str) -> bool {
    if key.is_empty() || key.starts_with('/') || key.contains('\\') || key.contains('\0') {
        return false;
    }
    if INVALID_KEYS.contains(&key) {
        return false;
    }
    key.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+'))
    })
}

// -------------------------------------------------------------------------
// Civil time
// -------------------------------------------------------------------------

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Civil date for a count of days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
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
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[derive(Clone, Copy)]
struct DateTime {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    microsecond: i64,
}

fn component(instance: i64, name: &str) -> Option<i64> {
    value::as_integer(value::instance_attr(instance, name)?)
}

fn datetime_from_value(v: i64) -> Option<DateTime> {
    value::as_instance(v)?;
    let dt = DateTime {
        year: component(v, "year")?,
        month: component(v, "month")?,
        day: component(v, "day")?,
        hour: component(v, "hour")?,
        minute: component(v, "minute")?,
        second: component(v, "second")?,
        microsecond: component(v, "microsecond").unwrap_or(0),
    };
    if !(1..=9_999).contains(&dt.year)
        || !(1..=12).contains(&dt.month)
        || !(1..=31).contains(&dt.day)
        || !(0..=23).contains(&dt.hour)
        || !(0..=59).contains(&dt.minute)
        || !(0..=59).contains(&dt.second)
    {
        return None;
    }
    Some(dt)
}

fn timestamp_of(dt: DateTime) -> i64 {
    days_from_civil(dt.year, dt.month, dt.day) * 86_400
        + dt.hour * 3_600
        + dt.minute * 60
        + dt.second
}

fn datetime_from_timestamp(seconds: i64, microsecond: i64) -> DateTime {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    DateTime {
        year,
        month,
        day,
        hour: rest / 3_600,
        minute: (rest % 3_600) / 60,
        second: rest % 60,
        microsecond,
    }
}

/// Builds a datetime-like instance of `dt`'s class with shifted components.
fn shifted_datetime(source: i64, seconds: i64) -> i64 {
    let Some(dt) = datetime_from_value(source) else {
        return type_error("fromutc() requires a datetime instance");
    };
    let shifted = datetime_from_timestamp(timestamp_of(dt) + seconds, dt.microsecond);
    let Some(class) = value::instance_class_value(source) else {
        return type_error("fromutc() requires a datetime instance");
    };
    let instance = value::instance_new(class);
    for (name, component) in [
        ("year", shifted.year),
        ("month", shifted.month),
        ("day", shifted.day),
        ("hour", shifted.hour),
        ("minute", shifted.minute),
        ("second", shifted.second),
        ("microsecond", shifted.microsecond),
    ] {
        value::instance_setattr(instance, name.to_string(), encode(component));
    }
    instance
}

// -------------------------------------------------------------------------
// ZoneInfo
// -------------------------------------------------------------------------

fn zoneinfo_class() -> i64 {
    if let Some(class) = crate::core::globals::registry_get("zoneinfo.ZoneInfo") {
        return class;
    }
    let methods = vec![
        ("__init__".to_string(), native(INIT, 3), MethodKind::Normal),
        (
            "utcoffset".to_string(),
            native(UTCOFFSET, 2),
            MethodKind::Normal,
        ),
        ("dst".to_string(), native(DST, 2), MethodKind::Normal),
        ("tzname".to_string(), native(TZNAME, 2), MethodKind::Normal),
        (
            "fromutc".to_string(),
            native(FROMUTC, 2),
            MethodKind::Normal,
        ),
        ("__str__".to_string(), native(STR, 1), MethodKind::Normal),
        ("__repr__".to_string(), native(REPR, 1), MethodKind::Normal),
    ];
    let attrs = vec![(
        "key".to_string(),
        value::property_new(native(GET_KEY, 1), NONE, NONE),
    )];
    let class = value::class_new_full("ZoneInfo".to_string(), NONE, methods, attrs);
    crate::core::globals::registry_set("zoneinfo.ZoneInfo", class);
    class
}

fn native(name: &'static str, arity: usize) -> i64 {
    value::native_function_new(MODULE, name, arity)
}

/// `ZoneInfo(key, cache=True)`.
fn zoneinfo_init(args: &[i64]) -> i64 {
    let instance = args[0];
    let Some(key) = value::as_str(args[1]) else {
        return type_error("ZoneInfo() key must be str");
    };
    // `cache` is accepted for signature compatibility; parsed zones are
    // always shared through the module cache, and instances are immutable.
    let _cache = args.get(2).copied().is_none_or(value::truthy);
    let key = key.to_string();
    if !valid_key(&key) {
        return not_found(&key);
    }
    if load_zone(&key).is_err() {
        return not_found(&key);
    }
    value::instance_setattr(instance, "_key".to_string(), value::str_alloc(key));
    NONE
}

fn zone_for(instance: i64) -> Result<Arc<Tzif>, i64> {
    let Some(key) = value::instance_attr(instance, "_key").and_then(value::as_str) else {
        return Err(type_error("ZoneInfo instance is not initialised"));
    };
    load_zone(key).map_err(|_| not_found(key))
}

fn zone_key(instance: i64) -> i64 {
    value::instance_attr(instance, "_key")
        .filter(|raw| value::as_str(*raw).is_some())
        .unwrap_or(NONE)
}

/// `ZoneInfo.utcoffset(dt)`: offset in seconds east of UTC.
fn utcoffset(args: &[i64]) -> i64 {
    let zone = match zone_for(args[0]) {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    if args[1] == NONE {
        return NONE;
    }
    let Some(dt) = datetime_from_value(args[1]) else {
        return type_error("utcoffset() requires a datetime instance or None");
    };
    encode(zone.offset_at_local(timestamp_of(dt)))
}

fn dst(args: &[i64]) -> i64 {
    let zone = match zone_for(args[0]) {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    if args[1] == NONE {
        return NONE;
    }
    let Some(dt) = datetime_from_value(args[1]) else {
        return type_error("dst() requires a datetime instance or None");
    };
    let local = timestamp_of(dt);
    let offset = zone.offset_at_local(local);
    encode(zone.dst_at(local.saturating_sub(offset)))
}

fn tzname(args: &[i64]) -> i64 {
    let zone = match zone_for(args[0]) {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    if args[1] == NONE {
        return NONE;
    }
    let Some(dt) = datetime_from_value(args[1]) else {
        return type_error("tzname() requires a datetime instance or None");
    };
    let local = timestamp_of(dt);
    let offset = zone.offset_at_local(local);
    let kind = zone.type_at(local.saturating_sub(offset));
    value::str_alloc(kind.abbr.clone())
}

fn fromutc(args: &[i64]) -> i64 {
    let zone = match zone_for(args[0]) {
        Ok(zone) => zone,
        Err(error) => return error,
    };
    let Some(dt) = datetime_from_value(args[1]) else {
        return type_error("fromutc() requires a datetime instance");
    };
    let utc = timestamp_of(dt);
    let offset = i64::from(zone.type_at(utc).offset);
    shifted_datetime(args[1], offset)
}

fn zone_str(args: &[i64]) -> i64 {
    let key = match value::as_str(zone_key(args[0])) {
        Some(key) => key.to_string(),
        None => return type_error("ZoneInfo instance is not initialised"),
    };
    value::str_alloc(format!("ZoneInfo(key='{key}')"))
}

// -------------------------------------------------------------------------
// Module functions
// -------------------------------------------------------------------------

/// `available_timezones()`: zone keys present under `TZPATH`.
fn available_timezones(_args: &[i64]) -> i64 {
    let mut names = Vec::new();
    for root in tzpath() {
        collect_zones(Path::new(&root), Path::new(&root), 0, &mut names);
    }
    names.sort();
    names.dedup();
    let items: Vec<i64> = names.into_iter().map(value::str_alloc).collect();
    value::set_new(items)
}

fn collect_zones(root: &Path, path: &Path, depth: usize, names: &mut Vec<String>) {
    if names.len() > 20_000 || depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if depth == 0 && (name == "posix" || name == "right") {
            continue;
        }
        let child = entry.path();
        let Ok(metadata) = std::fs::metadata(&child) else {
            continue;
        };
        if metadata.is_dir() {
            collect_zones(root, &child, depth + 1, names);
            continue;
        }
        if INVALID_KEYS.contains(&name) {
            continue;
        }
        let Ok(relative) = child.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let relative = relative.replace('\\', "/");
        if !valid_key(&relative) {
            continue;
        }
        if is_tzif_file(&child) {
            names.push(relative);
        }
    }
}

fn is_tzif_file(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read as _;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && &magic == b"TZif"
}

/// `reset_tzpath(to=None)`.
fn reset_tzpath(args: &[i64]) -> i64 {
    let replacement = match args.first().copied() {
        None | Some(NONE) => None,
        Some(raw) => {
            let items = if let Some(items) = value::as_list(raw) {
                items.to_vec()
            } else if let Some(items) = value::as_tuple(raw) {
                items.to_vec()
            } else {
                return type_error("reset_tzpath() argument must be a sequence of paths");
            };
            let mut paths = Vec::with_capacity(items.len());
            for item in items {
                let Some(text) = value::as_str(item) else {
                    return type_error("reset_tzpath() paths must be str");
                };
                if !Path::new(text).is_absolute() {
                    return exc::raise_with_message(
                        VALUE_ERROR,
                        "reset_tzpath() requires absolute paths",
                    );
                }
                paths.push(text.to_string());
            }
            Some(paths)
        }
    };
    *TZPATH_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = replacement;
    NONE
}

fn tzpath_value() -> i64 {
    let items: Vec<i64> = tzpath().into_iter().map(value::str_alloc).collect();
    value::tuple_new(items)
}

/// Dispatches a native `zoneinfo` function. Arity is enforced by the central
/// spec before this point.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "available_timezones" => available_timezones(args),
        "reset_tzpath" => reset_tzpath(args),
        INIT => zoneinfo_init(args),
        GET_KEY => zone_key(args[0]),
        UTCOFFSET => utcoffset(args),
        DST => dst(args),
        TZNAME => tzname(args),
        FROMUTC => fromutc(args),
        STR | REPR => zone_str(args),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'zoneinfo' has no attribute '{function}'"),
        ),
    }
}

/// Materialises a `zoneinfo` module constant.
pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ZoneInfo" => zoneinfo_class(),
        "ZoneInfoNotFoundError" => not_found_class(),
        "TZPATH" => tzpath_value(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module 'zoneinfo' has no attribute '{name}'"),
        ),
    }
}
