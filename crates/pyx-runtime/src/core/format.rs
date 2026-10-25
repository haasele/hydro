//! Python-compatible value formatting.
//!
//! Three entry points share one implementation of Python's format
//! mini-language:
//!
//! - [`format_value`] renders one interpolation field (used by f-strings
//!   through `pyx_format_value`),
//! - [`call_format`] implements `str.format`,
//! - [`percent_format`] implements `%` formatting for `str % args`.
//!
//! The mini-language parser is deliberately small but CPython-compatible for
//! the common cases: `[[fill]align][sign][#][0][width][,][.precision][type]`
//! with the types `s b c d o x X e E f F g G %`.
//!
//! No user input can panic here: every malformed spec becomes a
//! [`FormatError`] carrying the exception tag (`ValueError` for invalid
//! specs, `TypeError` for wrong operand types).

use crate::value;

/// A formatting failure with the exception tag the caller must raise.
#[derive(Clone, Debug)]
pub(crate) struct FormatError {
    pub(crate) tag: u32,
    pub(crate) message: String,
}

impl FormatError {
    fn key_error(message: impl Into<String>) -> FormatError {
        FormatError {
            tag: crate::exc::KEY_ERROR,
            message: message.into(),
        }
    }

    fn value_error(message: impl Into<String>) -> FormatError {
        FormatError {
            tag: crate::exc::VALUE_ERROR,
            message: message.into(),
        }
    }

    fn type_error(message: impl Into<String>) -> FormatError {
        FormatError {
            tag: crate::exc::TYPE_ERROR,
            message: message.into(),
        }
    }

    fn index_error(message: impl Into<String>) -> FormatError {
        FormatError {
            tag: crate::exc::INDEX_ERROR,
            message: message.into(),
        }
    }
}

/// Conversion flags of an f-string / `str.format` field.
const CONVERSION_NONE: u8 = 0;
const CONVERSION_STR: u8 = 1;
const CONVERSION_REPR: u8 = 2;
const CONVERSION_ASCII: u8 = 3;

// -------------------------------------------------------------------------
// Spec parsing
// -------------------------------------------------------------------------

/// Parsed `format_spec` mini-language.
#[derive(Clone, Debug, Default)]
struct Spec {
    fill: char,
    align: Option<char>,
    sign: Option<char>,
    alternate: bool,
    zero: bool,
    width: Option<usize>,
    grouping: bool,
    precision: Option<usize>,
    ty: Option<char>,
}

impl Spec {
    fn is_empty(&self) -> bool {
        self.align.is_none()
            && self.sign.is_none()
            && !self.alternate
            && !self.zero
            && self.width.is_none()
            && !self.grouping
            && self.precision.is_none()
            && self.ty.is_none()
    }
}

fn parse_spec(text: &str) -> Result<Spec, FormatError> {
    let chars: Vec<char> = text.chars().collect();
    let mut spec = Spec {
        fill: ' ',
        ..Spec::default()
    };
    let mut i = 0;

    let is_align = |c: char| matches!(c, '<' | '>' | '^' | '=');

    if chars.len() >= 2 && is_align(chars[1]) {
        spec.fill = chars[0];
        spec.align = Some(chars[1]);
        i = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        spec.align = Some(chars[0]);
        i = 1;
    }

    if i < chars.len() && matches!(chars[i], '+' | '-' | ' ') {
        spec.sign = Some(chars[i]);
        i += 1;
    }
    if chars.get(i) == Some(&'#') {
        spec.alternate = true;
        i += 1;
    }
    if chars.get(i) == Some(&'0') {
        spec.zero = true;
        i += 1;
    }

    let width_start = i;
    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    if i > width_start {
        let digits: String = chars[width_start..i].iter().collect();
        spec.width = digits.parse().ok();
    }

    if chars.get(i) == Some(&',') {
        spec.grouping = true;
        i += 1;
    }

    if chars.get(i) == Some(&'.') {
        i += 1;
        let precision_start = i;
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        let digits: String = chars[precision_start..i].iter().collect();
        spec.precision = Some(digits.parse().unwrap_or(0));
    }

    if i < chars.len() {
        spec.ty = Some(chars[i]);
        i += 1;
        if i != chars.len() {
            return Err(FormatError::value_error(format!(
                "Invalid format specifier '{text}'"
            )));
        }
    }

    Ok(spec)
}

fn unknown_code(ty: char, type_name: &str) -> FormatError {
    FormatError::value_error(format!(
        "Unknown format code '{ty}' for object of type '{type_name}'"
    ))
}

// -------------------------------------------------------------------------
// Public entry points
// -------------------------------------------------------------------------

/// Formats one value for an f-string field.
///
/// `conversion` is `0` (none), `1` (`!s`), `2` (`!r`) or `3` (`!a`); the
/// conversion is applied before the format spec, like CPython.
pub(crate) fn format_value(
    value: i64,
    spec_text: &str,
    conversion: u8,
) -> Result<String, FormatError> {
    let spec = parse_spec(spec_text)?;
    match conversion {
        CONVERSION_NONE => format_native(value, &spec),
        CONVERSION_STR => format_text(&value::to_string(value), &spec),
        CONVERSION_REPR => format_text(&value::repr(value), &spec),
        CONVERSION_ASCII => format_text(&ascii_repr(&value::repr(value)), &spec),
        _ => Err(FormatError::value_error("invalid conversion specifier")),
    }
}

/// `"template".format(args...)`.
pub(crate) fn call_format(template: &str, args: &[i64]) -> Result<String, FormatError> {
    format_template(template, args, &[])
}

/// `"template".format(args..., name=value...)`.
pub(crate) fn call_format_kw(
    template: &str,
    args: &[i64],
    named: &[(String, i64)],
) -> Result<String, FormatError> {
    format_template(template, args, named)
}

fn format_template(
    template: &str,
    args: &[i64],
    named: &[(String, i64)],
) -> Result<String, FormatError> {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len() + 16);
    let mut i = 0;
    let mut next_auto = 0usize;
    let mut used_auto = false;
    let mut used_manual = false;

    while i < chars.len() {
        match chars[i] {
            '{' => {
                if chars.get(i + 1) == Some(&'{') {
                    out.push('{');
                    i += 2;
                    continue;
                }
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '}' {
                    if chars[i] == '{' {
                        return Err(FormatError::value_error("unexpected '{' in field name"));
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(FormatError::value_error(
                        "Single '{' encountered in format string",
                    ));
                }
                let field: String = chars[start..i].iter().collect();
                i += 1; // closing '}'

                let (name, conversion, spec_text) = split_field(&field)?;
                let index = if name.is_empty() {
                    if used_manual {
                        return Err(FormatError::value_error(
                            "cannot switch from manual field specification to automatic field numbering",
                        ));
                    }
                    used_auto = true;
                    let index = next_auto;
                    next_auto += 1;
                    index
                } else if name.chars().all(|c| c.is_ascii_digit()) {
                    if used_auto {
                        return Err(FormatError::value_error(
                            "cannot switch from automatic field numbering to manual field specification",
                        ));
                    }
                    used_manual = true;
                    name.parse().unwrap_or(usize::MAX)
                } else {
                    let Some((_, value)) = named.iter().find(|(key, _)| key == &name) else {
                        return Err(FormatError::key_error(format!(
                            "str.format() missing named field '{name}'"
                        )));
                    };
                    out.push_str(&format_value(*value, &spec_text, conversion)?);
                    continue;
                };

                let Some(arg) = args.get(index).copied() else {
                    return Err(FormatError::index_error(format!(
                        "Replacement index {index} out of range for positional args tuple"
                    )));
                };
                out.push_str(&format_value(arg, &spec_text, conversion)?);
            }
            '}' => {
                if chars.get(i + 1) == Some(&'}') {
                    out.push('}');
                    i += 2;
                } else {
                    return Err(FormatError::value_error(
                        "Single '}' encountered in format string",
                    ));
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Splits a `str.format` replacement field into name, conversion and spec.
fn split_field(field: &str) -> Result<(String, u8, String), FormatError> {
    let mut name_end = field.len();
    let mut conversion = CONVERSION_NONE;
    let mut spec_start: Option<usize> = None;

    let chars: Vec<char> = field.chars().collect();
    let mut i = 0;
    let mut byte_index = 0usize;
    while i < chars.len() {
        match chars[i] {
            '!' => {
                name_end = byte_index;
                let conversion_char = chars.get(i + 1).copied().ok_or_else(|| {
                    FormatError::value_error("end of string while looking for conversion specifier")
                })?;
                conversion = match conversion_char {
                    's' => CONVERSION_STR,
                    'r' => CONVERSION_REPR,
                    'a' => CONVERSION_ASCII,
                    other => {
                        return Err(FormatError::value_error(format!(
                            "Unknown conversion specifier {other}"
                        )));
                    }
                };
                i += 2;
                byte_index += 2;
                if chars.get(i) == Some(&':') {
                    spec_start = Some(byte_index + 1);
                } else if i != chars.len() {
                    return Err(FormatError::value_error(format!(
                        "Invalid conversion specifier in field '{field}'"
                    )));
                }
                break;
            }
            ':' => {
                name_end = byte_index;
                spec_start = Some(byte_index + 1);
                break;
            }
            _ => {
                i += 1;
                byte_index += chars[i - 1].len_utf8();
            }
        }
    }

    let name = field[..name_end].to_string();
    if name.contains('{') || name.contains('}') {
        return Err(FormatError::value_error(
            "dynamic format specs are not supported",
        ));
    }
    let spec = match spec_start {
        Some(start) => {
            let spec = field[start..].to_string();
            if spec.contains('{') || spec.contains('}') {
                return Err(FormatError::value_error(
                    "dynamic format specs are not supported",
                ));
            }
            spec
        }
        None => String::new(),
    };
    Ok((name, conversion, spec))
}

/// `"template" % rhs`.
pub(crate) fn percent_format(template: &str, rhs: i64) -> Result<String, FormatError> {
    let args: Vec<i64> = match value::as_tuple(rhs) {
        Some(items) => items.to_vec(),
        None => vec![rhs],
    };

    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len() + 16);
    let mut i = 0;
    let mut next_arg = 0usize;

    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'%') {
            out.push('%');
            i += 2;
            continue;
        }
        i += 1;
        if chars.get(i) == Some(&'(') {
            return Err(FormatError::type_error(
                "format requires a mapping (named %()s keys are not supported)",
            ));
        }

        let mut flags = String::new();
        while chars
            .get(i)
            .is_some_and(|c| matches!(*c, '#' | '0' | '-' | ' ' | '+'))
        {
            flags.push(chars[i]);
            i += 1;
        }
        let width_start = i;
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        let width: usize = chars[width_start..i]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if chars.get(i) == Some(&'*') {
            return Err(FormatError::value_error(
                "unsupported '*' width in percent format",
            ));
        }

        let mut precision: Option<usize> = None;
        if chars.get(i) == Some(&'.') {
            i += 1;
            let precision_start = i;
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                i += 1;
            }
            precision = Some(
                chars[precision_start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0),
            );
        }
        while chars.get(i).is_some_and(|c| matches!(*c, 'h' | 'l' | 'L')) {
            i += 1;
        }
        let Some(ty) = chars.get(i).copied() else {
            return Err(FormatError::value_error("incomplete format"));
        };
        i += 1;
        if ty == '%' {
            out.push('%');
            continue;
        }

        let Some(arg) = args.get(next_arg).copied() else {
            return Err(FormatError::type_error(
                "not enough arguments for format string",
            ));
        };
        next_arg += 1;

        let piece = percent_piece(arg, ty, precision, i - 1, &flags)?;
        out.push_str(&percent_pad(&piece, width, &flags));
    }

    // CPython accepts a dict right operand for a template without mapping
    // keys even though it consumed no argument.
    let mapping_operand = next_arg == 0 && value::as_dict(rhs).is_some();
    if next_arg < args.len() && !mapping_operand {
        return Err(FormatError::type_error(
            "not all arguments converted during string formatting",
        ));
    }
    Ok(out)
}

// -------------------------------------------------------------------------
// Native value formatting
// -------------------------------------------------------------------------

fn format_native(value: i64, spec: &Spec) -> Result<String, FormatError> {
    if value::is_special(value) {
        return match value >> 3 {
            // `False`/`True`: an empty spec renders the name, any other spec
            // formats the underlying integer.
            1 | 2 => {
                if spec.is_empty() {
                    Ok(value::to_string(value))
                } else if spec.ty == Some('s') {
                    Err(unknown_code('s', "bool"))
                } else {
                    format_int(value::as_integer(value).unwrap_or(0), spec, "bool")
                }
            }
            _ => format_other(value, spec),
        };
    }
    if value::is_int(value) {
        return format_int(value >> 3, spec, "int");
    }
    if let Some(number) = value::as_bigint(value) {
        return format_bigint(number, spec);
    }
    if let Some(f) = value::as_float(value) {
        return format_float_value(f, spec);
    }
    if let Some(s) = value::as_str(value) {
        return format_text(s, spec);
    }
    format_other(value, spec)
}

/// Types without `__format__`: only the empty spec is accepted.
fn format_other(value: i64, spec: &Spec) -> Result<String, FormatError> {
    if spec.is_empty() {
        return Ok(value::to_string(value));
    }
    Err(FormatError::type_error(format!(
        "unsupported format string passed to {}.__format__",
        value::type_name(value)
    )))
}

fn format_text(text: &str, spec: &Spec) -> Result<String, FormatError> {
    if let Some(ty) = spec.ty {
        if ty != 's' {
            return Err(unknown_code(ty, "str"));
        }
    }
    if spec.sign.is_some() {
        return Err(FormatError::value_error(
            "Sign not allowed in string format specifier",
        ));
    }
    if spec.alternate {
        return Err(FormatError::value_error(
            "Alternate form (#) not allowed in string format specifier",
        ));
    }
    if spec.grouping {
        return Err(FormatError::value_error("Cannot specify ',' with 's'."));
    }
    if spec.align == Some('=') {
        return Err(FormatError::value_error(
            "'=' alignment not allowed in string format specifier",
        ));
    }

    let body: String = match spec.precision {
        Some(precision) => text.chars().take(precision).collect(),
        None => text.to_string(),
    };
    pad("", &body, "", spec, '<', false)
}

fn format_int(number: i64, spec: &Spec, type_name: &str) -> Result<String, FormatError> {
    let ty = spec.ty.unwrap_or('d');

    // Integers accept the float presentation types.
    if matches!(ty, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
        return format_float_value(number as f64, spec);
    }
    if ty == 'c' {
        if spec.sign.is_some() || spec.grouping || spec.precision.is_some() {
            return Err(FormatError::value_error(
                "invalid format specifier for character format",
            ));
        }
        let Some(character) = u32::try_from(number).ok().and_then(char::from_u32) else {
            return Err(FormatError::value_error(
                "character code out of range for 'c' format",
            ));
        };
        let mut text_spec = spec.clone();
        text_spec.ty = None;
        return format_text(&character.to_string(), &text_spec);
    }
    if !matches!(ty, 'd' | 'b' | 'o' | 'x' | 'X') {
        return Err(unknown_code(ty, type_name));
    }
    if spec.precision.is_some() {
        return Err(FormatError::value_error(
            "Precision not allowed in integer format specifier",
        ));
    }
    if spec.grouping && ty != 'd' {
        return Err(FormatError::value_error(format!(
            "Cannot specify ',' with '{ty}'."
        )));
    }
    let negative = number < 0;
    let magnitude = number.unsigned_abs();
    let body = match ty {
        'b' => format!("{magnitude:b}"),
        'o' => format!("{magnitude:o}"),
        'x' => format!("{magnitude:x}"),
        'X' => format!("{magnitude:X}"),
        _ => magnitude.to_string(),
    };
    let body = if spec.grouping {
        group_int(&body, ',')
    } else {
        body
    };
    let base_prefix = match ty {
        'b' if spec.alternate => "0b",
        'o' if spec.alternate => "0o",
        'x' if spec.alternate => "0x",
        'X' if spec.alternate => "0X",
        _ => "",
    };
    let sign = if negative {
        "-"
    } else {
        match spec.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        }
    };
    let prefix = format!("{sign}{base_prefix}");
    pad(&prefix, &body, "", spec, '>', true)
}

/// Formats an arbitrary-precision integer. Only the integer presentation
/// types (`b c d o x X`) are computed exactly; the float presentation types
/// fall back to the float path when the value fits an `f64`.
fn format_bigint(number: &num_bigint::BigInt, spec: &Spec) -> Result<String, FormatError> {
    let ty = spec.ty.unwrap_or('d');
    if matches!(ty, 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
        let Some(as_float) = value::bigint_to_f64(number) else {
            return Err(FormatError::value_error(
                "int too large to convert to float",
            ));
        };
        return format_float_value(as_float, spec);
    }
    if ty == 'c' {
        if spec.sign.is_some() || spec.grouping || spec.precision.is_some() {
            return Err(FormatError::value_error(
                "invalid format specifier for character format",
            ));
        }
        let Some(character) = u32::try_from(number).ok().and_then(char::from_u32) else {
            return Err(FormatError::value_error(
                "character code out of range for 'c' format",
            ));
        };
        let mut text_spec = spec.clone();
        text_spec.ty = None;
        return format_text(&character.to_string(), &text_spec);
    }
    if !matches!(ty, 'd' | 'b' | 'o' | 'x' | 'X') {
        return Err(unknown_code(ty, "int"));
    }
    if spec.precision.is_some() {
        return Err(FormatError::value_error(
            "Precision not allowed in integer format specifier",
        ));
    }
    if spec.grouping && ty != 'd' {
        return Err(FormatError::value_error(format!(
            "Cannot specify ',' with '{ty}'."
        )));
    }
    let negative = number.sign() == num_bigint::Sign::Minus;
    let radix = match ty {
        'b' => 2,
        'o' => 8,
        'x' | 'X' => 16,
        _ => 10,
    };
    let mut body = number.magnitude().to_str_radix(radix);
    if ty == 'X' {
        body.make_ascii_uppercase();
    }
    let body = if spec.grouping {
        group_int(&body, ',')
    } else {
        body
    };
    let base_prefix = match ty {
        'b' if spec.alternate => "0b",
        'o' if spec.alternate => "0o",
        'x' if spec.alternate => "0x",
        'X' if spec.alternate => "0X",
        _ => "",
    };
    let sign = if negative {
        "-"
    } else {
        match spec.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        }
    };
    let prefix = format!("{sign}{base_prefix}");
    pad(&prefix, &body, "", spec, '>', true)
}

fn format_float_value(number: f64, spec: &Spec) -> Result<String, FormatError> {
    let ty = spec.ty.unwrap_or('\0');
    if !matches!(ty, '\0' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' | '%') {
        return Err(unknown_code(ty, "float"));
    }
    let upper = matches!(ty, 'E' | 'F' | 'G');
    let negative = number.is_sign_negative() && !number.is_nan();
    let magnitude = number.abs();
    let precision = spec.precision.unwrap_or(6);

    let special_suffix = if ty == '%' { "%" } else { "" };
    let (body, suffix) = if number.is_nan() {
        (
            if upper {
                "NAN".to_string()
            } else {
                "nan".to_string()
            },
            special_suffix.to_string(),
        )
    } else if number.is_infinite() {
        (
            if upper {
                "INF".to_string()
            } else {
                "inf".to_string()
            },
            special_suffix.to_string(),
        )
    } else {
        match ty {
            'f' | 'F' => (format!("{:.*}", precision, magnitude), String::new()),
            '%' => (
                format!("{:.*}", precision, magnitude * 100.0),
                "%".to_string(),
            ),
            'e' | 'E' => (scientific(magnitude, precision, upper), String::new()),
            'g' | 'G' => (
                general(magnitude, precision, upper, spec.alternate),
                String::new(),
            ),
            _ => match spec.precision {
                Some(precision) => (
                    general(magnitude, precision, false, spec.alternate),
                    String::new(),
                ),
                None => (value::format_float(magnitude), String::new()),
            },
        }
    };
    let body = if spec.grouping {
        group_number(&body, ',')
    } else {
        body
    };
    let sign = if negative {
        "-"
    } else {
        match spec.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        }
    };
    let prefix = sign.to_string();
    pad(&prefix, &body, &suffix, spec, '>', true)
}

/// Scientific notation with CPython's exponent spelling (`e+05`).
fn scientific(magnitude: f64, precision: usize, upper: bool) -> String {
    let raw = format!("{:.*e}", precision, magnitude);
    let (mantissa, exponent) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let marker = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}{marker}{sign}{:02}", exponent.abs())
}

/// Python's `g` presentation: significant digits with optional exponent.
fn general(magnitude: f64, precision: usize, upper: bool, alternate: bool) -> String {
    let precision = precision.max(1);
    let raw = format!("{:.*e}", precision - 1, magnitude);
    let (mantissa, exponent) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = if digits.is_empty() {
        "0".to_string()
    } else {
        digits
    };

    if exponent < -4 || exponent >= precision as i32 {
        let head = &digits[..1];
        let tail = &digits[1..];
        let mut out = String::from(head);
        let tail_trimmed = tail.trim_end_matches('0');
        if alternate || !tail_trimmed.is_empty() {
            out.push('.');
            out.push_str(if alternate { tail } else { tail_trimmed });
        }
        out.push(if upper { 'E' } else { 'e' });
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.abs()));
        return out;
    }

    let int_len = exponent + 1;
    let (int_part, frac_part) = if int_len <= 0 {
        (
            "0".to_string(),
            format!("{}{}", "0".repeat((-int_len) as usize), digits),
        )
    } else if int_len as usize >= digits.len() {
        (
            format!("{}{}", digits, "0".repeat(int_len as usize - digits.len())),
            String::new(),
        )
    } else {
        (
            digits[..int_len as usize].to_string(),
            digits[int_len as usize..].to_string(),
        )
    };
    let frac_trimmed = frac_part.trim_end_matches('0');
    let mut out = int_part;
    if alternate {
        out.push('.');
        out.push_str(&frac_part);
    } else if !frac_trimmed.is_empty() {
        out.push('.');
        out.push_str(frac_trimmed);
    }
    out
}

// -------------------------------------------------------------------------
// Padding and grouping
// -------------------------------------------------------------------------

/// Applies fill, alignment and width.
///
/// `prefix` holds the sign and base prefix, `suffix` the percent sign. `=`
/// padding goes between the prefix and the body; `zero` only selects `=`
/// padding for numeric values (strings keep their default alignment and pad
/// with zeros, as CPython does).
fn pad(
    prefix: &str,
    body: &str,
    suffix: &str,
    spec: &Spec,
    default_align: char,
    numeric: bool,
) -> Result<String, FormatError> {
    let mut align = spec.align.unwrap_or(default_align);
    let mut fill = spec.fill;
    if spec.zero && spec.align.is_none() {
        fill = '0';
        if numeric {
            align = '=';
        }
    }
    if align == '=' && !numeric {
        return Err(FormatError::value_error(
            "'=' alignment not allowed in string format specifier",
        ));
    }

    let length = prefix.chars().count() + body.chars().count() + suffix.chars().count();
    let width = spec.width.unwrap_or(0);
    if width <= length {
        return Ok(format!("{prefix}{body}{suffix}"));
    }
    let padding = width - length;
    Ok(match align {
        '<' => format!("{prefix}{body}{suffix}{}", fill.to_string().repeat(padding)),
        '>' => format!("{}{prefix}{body}{suffix}", fill.to_string().repeat(padding)),
        '^' => {
            let left = padding / 2;
            let right = padding - left;
            format!(
                "{}{prefix}{body}{suffix}{}",
                fill.to_string().repeat(left),
                fill.to_string().repeat(right)
            )
        }
        _ => format!("{prefix}{}{body}{suffix}", fill.to_string().repeat(padding)),
    })
}

/// Inserts `separator` every three digits from the right.
fn group_int(digits: &str, separator: char) -> String {
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return digits.to_string();
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let offset = digits.len() % 3;
    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (index + 3 - offset).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(byte as char);
    }
    out
}

/// Groups the integer part of a rendered number, leaving fraction and
/// exponent untouched.
fn group_number(text: &str, separator: char) -> String {
    let exponent_at = text.find(['e', 'E']).unwrap_or(text.len());
    let (mantissa, exponent) = text.split_at(exponent_at);
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((int_part, frac_part)) => (int_part, frac_part),
        None => (mantissa, ""),
    };
    let mut out = group_int(int_part, separator);
    if !frac_part.is_empty() {
        out.push('.');
        out.push_str(frac_part);
    }
    out.push_str(exponent);
    out
}

/// `ascii()` escaping of a `repr` string: non-ASCII characters become
/// `\xNN`, `\uNNNN` or `\U000NNNNN` escapes.
fn ascii_repr(repr: &str) -> String {
    let mut out = String::with_capacity(repr.len() + 8);
    for character in repr.chars() {
        let code = character as u32;
        if code < 0x80 {
            out.push(character);
        } else if code <= 0xff {
            out.push_str(&format!("\\x{code:02x}"));
        } else if code <= 0xffff {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            out.push_str(&format!("\\U{code:08x}"));
        }
    }
    out
}

// -------------------------------------------------------------------------
// Percent formatting
// -------------------------------------------------------------------------

/// One rendered `%` conversion, split so width/sign padding can be applied.
struct Piece {
    prefix: String,
    body: String,
    suffix: String,
    numeric: bool,
}

fn percent_piece(
    arg: i64,
    ty: char,
    precision: Option<usize>,
    index: usize,
    flags: &str,
) -> Result<Piece, FormatError> {
    match ty {
        's' | 'r' | 'a' => {
            let mut body = match ty {
                's' => value::to_string(arg),
                'r' => value::repr(arg),
                _ => ascii_repr(&value::repr(arg)),
            };
            if let Some(precision) = precision {
                body = body.chars().take(precision).collect();
            }
            Ok(Piece {
                prefix: String::new(),
                body,
                suffix: String::new(),
                numeric: false,
            })
        }
        'd' | 'i' | 'u' => {
            // Arbitrary-precision integers keep every digit.
            if let Some(number) = value::as_bigint(arg) {
                let mut digits = number.magnitude().to_str_radix(10);
                if let Some(precision) = precision {
                    if digits.len() < precision {
                        digits = format!("{}{digits}", "0".repeat(precision - digits.len()));
                    }
                }
                return Ok(Piece {
                    prefix: percent_prefix(number.sign() == num_bigint::Sign::Minus, flags, ""),
                    body: digits,
                    suffix: String::new(),
                    numeric: true,
                });
            }
            let number = percent_integer(arg, ty)?;
            let mut digits = number.unsigned_abs().to_string();
            if let Some(precision) = precision {
                if digits.len() < precision {
                    digits = format!("{}{digits}", "0".repeat(precision - digits.len()));
                }
            }
            Ok(Piece {
                prefix: percent_prefix(number.is_negative(), flags, ""),
                body: digits,
                suffix: String::new(),
                numeric: true,
            })
        }
        'x' | 'X' | 'o' => {
            let number = percent_index(arg, ty)?;
            let magnitude = number.unsigned_abs();
            let digits = match ty {
                'x' => format!("{magnitude:x}"),
                'X' => format!("{magnitude:X}"),
                _ => format!("{magnitude:o}"),
            };
            let digits = match precision {
                Some(precision) if digits.len() < precision => {
                    format!("{}{digits}", "0".repeat(precision - digits.len()))
                }
                _ => digits,
            };
            let base_prefix = if flags.contains('#') {
                match ty {
                    'x' => "0x",
                    'X' => "0X",
                    _ => "0o",
                }
            } else {
                ""
            };
            Ok(Piece {
                prefix: percent_prefix(number.is_negative(), flags, base_prefix),
                body: digits,
                suffix: String::new(),
                numeric: true,
            })
        }
        'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
            let number = percent_float(arg, ty)?;
            let precision = precision.unwrap_or(6);
            let upper = matches!(ty, 'F' | 'E' | 'G');
            let magnitude = number.abs();
            let body = if number.is_nan() {
                if upper {
                    "NAN".to_string()
                } else {
                    "nan".to_string()
                }
            } else if number.is_infinite() {
                if upper {
                    "INF".to_string()
                } else {
                    "inf".to_string()
                }
            } else {
                match ty {
                    'f' | 'F' => format!("{:.*}", precision, magnitude),
                    'e' | 'E' => scientific(magnitude, precision, upper),
                    _ => general(magnitude, precision, upper, false),
                }
            };
            Ok(Piece {
                prefix: percent_prefix(number.is_sign_negative() && !number.is_nan(), flags, ""),
                body,
                suffix: String::new(),
                numeric: true,
            })
        }
        'c' => {
            let body = if let Some(number) = value::as_integer(arg) {
                u32::try_from(number)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .ok_or_else(|| FormatError::value_error("%c arg not in range(0x110000)"))?
            } else if let Some(text) = value::as_str(arg) {
                if text.chars().count() == 1 {
                    text.to_string()
                } else {
                    return Err(FormatError::type_error("%c requires int or char"));
                }
            } else {
                return Err(FormatError::type_error("%c requires int or char"));
            };
            Ok(Piece {
                prefix: String::new(),
                body,
                suffix: String::new(),
                numeric: false,
            })
        }
        other => Err(FormatError::value_error(format!(
            "unsupported format character '{other}' (0x{:x}) at index {index}",
            other as u32
        ))),
    }
}

fn percent_prefix(negative: bool, flags: &str, base_prefix: &str) -> String {
    let sign = if negative {
        "-"
    } else if flags.contains('+') {
        "+"
    } else if flags.contains(' ') {
        " "
    } else {
        ""
    };
    format!("{sign}{base_prefix}")
}

fn percent_integer(arg: i64, ty: char) -> Result<i64, FormatError> {
    if let Some(number) = value::as_integer(arg) {
        return Ok(number);
    }
    if let Some(number) = value::as_float(arg) {
        // CPython truncates floats for `%d`/`%i`.
        return Ok(number.trunc() as i64);
    }
    Err(FormatError::type_error(format!(
        "%{ty} format: a real number is required, not {}",
        value::type_name(arg)
    )))
}

fn percent_index(arg: i64, ty: char) -> Result<i64, FormatError> {
    value::as_integer(arg).ok_or_else(|| {
        FormatError::type_error(format!(
            "%{ty} format: an integer is required, not {}",
            value::type_name(arg)
        ))
    })
}

fn percent_float(arg: i64, ty: char) -> Result<f64, FormatError> {
    if let Some(number) = value::as_integer(arg) {
        return Ok(number as f64);
    }
    if let Some(number) = value::as_float(arg) {
        return Ok(number);
    }
    Err(FormatError::type_error(format!(
        "%{ty} format: a real number is required, not {}",
        value::type_name(arg)
    )))
}

fn percent_pad(piece: &Piece, width: usize, flags: &str) -> String {
    let length =
        piece.prefix.chars().count() + piece.body.chars().count() + piece.suffix.chars().count();
    if width <= length {
        return format!("{}{}{}", piece.prefix, piece.body, piece.suffix);
    }
    let padding = width - length;
    if piece.numeric && flags.contains('0') && !flags.contains('-') {
        return format!(
            "{}{}{}{}",
            piece.prefix,
            "0".repeat(padding),
            piece.body,
            piece.suffix
        );
    }
    if flags.contains('-') {
        format!(
            "{}{}{}{}",
            piece.prefix,
            piece.body,
            piece.suffix,
            " ".repeat(padding)
        )
    } else {
        format!(
            "{}{}{}{}",
            " ".repeat(padding),
            piece.prefix,
            piece.body,
            piece.suffix
        )
    }
}

#[cfg(test)]
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;
    use crate::value::{self, encode_int, FALSE, NONE, TRUE};

    fn int(number: i64) -> i64 {
        encode_int(number).expect("representable")
    }

    fn float(number: f64) -> i64 {
        value::float_new(number)
    }

    fn string(text: &str) -> i64 {
        value::str_from_bytes(text.as_bytes())
    }

    fn list(items: &[i64]) -> i64 {
        value::list_new(items.to_vec())
    }

    fn tuple(items: &[i64]) -> i64 {
        value::tuple_new(items.to_vec())
    }

    fn dict(pairs: &[(i64, i64)]) -> i64 {
        value::dict_new(pairs.to_vec())
    }

    fn text(result: Result<String, FormatError>) -> String {
        result.expect("formatting must succeed")
    }

    fn error_tag(result: Result<String, FormatError>) -> u32 {
        result.expect_err("formatting must fail").tag
    }

    #[test]
    fn integer_format_matrix() {
        assert_eq!(text(format_value(int(42), "04d", 0)), "0042");
        assert_eq!(text(format_value(int(-42), "08d", 0)), "-0000042");
        assert_eq!(text(format_value(int(42), "+d", 0)), "+42");
        assert_eq!(text(format_value(int(42), " d", 0)), " 42");
        assert_eq!(text(format_value(int(255), "#x", 0)), "0xff");
        assert_eq!(text(format_value(int(255), "X", 0)), "FF");
        assert_eq!(text(format_value(int(8), "#o", 0)), "0o10");
        assert_eq!(text(format_value(int(5), "#b", 0)), "0b101");
        assert_eq!(text(format_value(int(1234567), ",d", 0)), "1,234,567");
        assert_eq!(text(format_value(int(65), "c", 0)), "A");
        assert_eq!(
            error_tag(format_value(int(5), "5s", 0)),
            crate::exc::VALUE_ERROR
        );
    }

    #[test]
    fn bigint_format_matrix() {
        let big = |literal: &str| {
            value::bigint_new(literal.parse::<num_bigint::BigInt>().expect("literal"))
        };
        let two_100 = big("1267650600228229401496703205376");
        assert_eq!(
            text(format_value(two_100, "", 0)),
            "1267650600228229401496703205376"
        );
        assert_eq!(
            text(format_value(two_100, "d", 0)),
            "1267650600228229401496703205376"
        );
        assert_eq!(
            text(format_value(two_100, ",d", 0)),
            "1,267,650,600,228,229,401,496,703,205,376"
        );
        assert_eq!(
            text(format_value(two_100, "x", 0)),
            "10000000000000000000000000"
        );
        assert_eq!(
            text(format_value(two_100, "#x", 0)),
            "0x10000000000000000000000000"
        );
        assert_eq!(
            text(format_value(two_100, "040", 0)),
            "0000000001267650600228229401496703205376"
        );

        let negative = big("-1267650600228229401496703205376");
        assert_eq!(
            text(format_value(negative, ",d", 0)),
            "-1,267,650,600,228,229,401,496,703,205,376"
        );
        assert_eq!(
            text(format_value(negative, "+d", 0)),
            "-1267650600228229401496703205376"
        );
        assert_eq!(
            text(format_value(two_100, "#X", 0)),
            "0X10000000000000000000000000"
        );
        assert_eq!(
            error_tag(format_value(two_100, "5s", 0)),
            crate::exc::VALUE_ERROR
        );
    }

    #[test]
    fn float_format_matrix() {
        assert_eq!(text(format_value(float(3.14159), ".2f", 0)), "3.14");
        assert_eq!(text(format_value(float(-3.5), "010.2f", 0)), "-000003.50");
        assert_eq!(text(format_value(float(3.14159), ".1e", 0)), "3.1e+00");
        assert_eq!(text(format_value(float(123456.0), ".2E", 0)), "1.23E+05");
        assert_eq!(text(format_value(float(3.5), "%", 0)), "350.000000%");
        assert_eq!(text(format_value(float(3.5), ".1%", 0)), "350.0%");
        assert_eq!(text(format_value(float(123456789.0), ".3g", 0)), "1.23e+08");
        assert_eq!(text(format_value(float(0.00001234), "g", 0)), "1.234e-05");
        assert_eq!(text(format_value(float(1234.5678), ",.2f", 0)), "1,234.57");
        assert_eq!(text(format_value(float(3.0), "", 0)), "3.0");
        assert_eq!(text(format_value(float(3.14159), ".2", 0)), "3.1");
        assert_eq!(
            error_tag(format_value(float(3.5), "d", 0)),
            crate::exc::VALUE_ERROR
        );
    }

    #[test]
    fn string_and_alignment_matrix() {
        assert_eq!(text(format_value(string("abc"), ">5", 0)), "  abc");
        assert_eq!(text(format_value(string("abc"), "<5", 0)), "abc  ");
        assert_eq!(text(format_value(string("abc"), "^6", 0)), " abc  ");
        assert_eq!(text(format_value(string("abc"), ".2", 0)), "ab");
        assert_eq!(text(format_value(string("x"), "05", 0)), "x0000");
        assert_eq!(
            error_tag(format_value(string("abc"), ",", 0)),
            crate::exc::VALUE_ERROR
        );
        assert_eq!(
            error_tag(format_value(string("abc"), "+", 0)),
            crate::exc::VALUE_ERROR
        );
    }

    #[test]
    fn bool_and_container_rendering() {
        assert_eq!(text(format_value(TRUE, "", 0)), "True");
        assert_eq!(text(format_value(FALSE, "", 0)), "False");
        assert_eq!(text(format_value(TRUE, ">5", 0)), "    1");
        assert_eq!(text(format_value(TRUE, "d", 0)), "1");
        assert_eq!(text(format_value(NONE, "", 0)), "None");
        assert_eq!(text(format_value(list(&[int(1), int(2)]), "", 0)), "[1, 2]");
        assert_eq!(
            text(format_value(dict(&[(string("k"), int(1))]), "", 0)),
            "{'k': 1}"
        );
        assert_eq!(
            error_tag(format_value(list(&[int(1)]), ">5", 0)),
            crate::exc::TYPE_ERROR
        );
    }

    #[test]
    fn conversions_apply_before_the_spec() {
        assert_eq!(text(format_value(string("x"), "", 2)), "'x'");
        assert_eq!(text(format_value(string("x"), ">5", 2)), "  'x'");
        assert_eq!(text(format_value(string("é"), "", 3)), "'\\xe9'");
        assert_eq!(text(format_value(string("😀"), "", 3)), "'\\U0001f600'");
        assert_eq!(text(format_value(int(3), "", 1)), "3");
        assert_eq!(text(format_value(TRUE, "", 2)), "True");
    }

    #[test]
    fn str_format_fields() {
        assert_eq!(
            text(call_format("{} {:.1f}", &[string("a"), float(2.25)])),
            "a 2.2"
        );
        assert_eq!(
            text(call_format("{1} {0}", &[string("a"), string("b")])),
            "b a"
        );
        assert_eq!(text(call_format("{{x}} {}", &[int(1)])), "{x} 1");
        assert_eq!(text(call_format("{!r}", &[string("x")])), "'x'");
        assert_eq!(text(call_format("{!a}", &[string("é")])), "'\\xe9'");
        assert_eq!(text(call_format("{0:04d}", &[int(42)])), "0042");
        assert_eq!(text(call_format("{0} {0}", &[int(5)])), "5 5");
        assert_eq!(
            text(call_format_kw(
                "{name}: {value:04d}",
                &[],
                &[
                    ("name".to_string(), string("x")),
                    ("value".to_string(), int(7))
                ]
            )),
            "x: 0007"
        );
        assert_eq!(
            error_tag(call_format("{name}", &[int(1)])),
            crate::exc::KEY_ERROR
        );
        assert_eq!(error_tag(call_format("{}", &[])), crate::exc::INDEX_ERROR);
        assert_eq!(
            error_tag(call_format("{0} {}", &[int(1), int(2)])),
            crate::exc::VALUE_ERROR
        );
        assert_eq!(error_tag(call_format("{", &[])), crate::exc::VALUE_ERROR);
    }

    #[test]
    fn percent_format_fields() {
        assert_eq!(
            text(percent_format("%s:%d", tuple(&[string("x"), int(3)]))),
            "x:3"
        );
        assert_eq!(text(percent_format("%s", int(3))), "3");
        assert_eq!(text(percent_format("%05.2f", float(3.14159))), "03.14");
        assert_eq!(text(percent_format("%r", string("x"))), "'x'");
        assert_eq!(text(percent_format("%a", string("é"))), "'\\xe9'");
        assert_eq!(text(percent_format("%c", int(65))), "A");
        assert_eq!(text(percent_format("%c", string("x"))), "x");
        assert_eq!(text(percent_format("%%", tuple(&[]))), "%");
        assert_eq!(text(percent_format("%#x", int(255))), "0xff");
        assert_eq!(text(percent_format("%#x", int(-255))), "-0xff");
        assert_eq!(text(percent_format("%d", float(3.7))), "3");

        // Arbitrary-precision integers render every digit.
        let big = value::bigint_new("1267650600228229401496703205376".parse().unwrap());
        assert_eq!(
            text(percent_format("%d", big)),
            "1267650600228229401496703205376"
        );
        let negative_big = value::bigint_new("-123456789012345678901234567890".parse().unwrap());
        assert_eq!(
            text(percent_format("%+d", negative_big)),
            "-123456789012345678901234567890"
        );
        assert_eq!(
            error_tag(percent_format("%s %s", tuple(&[string("a")]))),
            crate::exc::TYPE_ERROR
        );
        assert_eq!(
            error_tag(percent_format("%s", tuple(&[int(1), int(2)]))),
            crate::exc::TYPE_ERROR
        );
        assert_eq!(
            error_tag(percent_format("%d", string("x"))),
            crate::exc::TYPE_ERROR
        );
        assert_eq!(
            error_tag(percent_format("%q", int(1))),
            crate::exc::VALUE_ERROR
        );
    }
}
