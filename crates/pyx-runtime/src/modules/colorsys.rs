//! `colorsys`: conversions between RGB and the YIQ, HLS and HSV systems.
//!
//! The formulas and their floating-point constants mirror CPython's
//! `Lib/colorsys.py` exactly, including the HLS saturation branch for
//! near-white colours and the YIQ clamping. Inputs outside the documented
//! range produce whatever the formulas produce, as in CPython.

use crate::exc::{self, ATTRIBUTE_ERROR, TYPE_ERROR};
use crate::value::{self, Num};

const ONE_THIRD: f64 = 1.0 / 3.0;
const ONE_SIXTH: f64 = 1.0 / 6.0;
const TWO_THIRD: f64 = 2.0 / 3.0;

/// CPython's float `%`: the result takes the sign of the divisor.
fn mod_one(x: f64) -> f64 {
    x.rem_euclid(1.0)
}

/// Numeric view of a colour component: int, bool and float are accepted.
fn number(v: i64) -> Result<f64, i64> {
    match value::num_of(v) {
        Some(Num::Int(i)) => Ok(i as f64),
        Some(Num::Float(f)) => Ok(f),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!("must be real number, not {}", value::type_name(v)),
        )),
    }
}

/// Dispatches one of the six conversion functions. Every function takes three
/// components and returns a fresh three-tuple of floats.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    if args.len() != 3 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "colorsys.{function}() takes 3 arguments ({} given)",
                args.len()
            ),
        );
    }
    let mut values = [0.0f64; 3];
    for (slot, arg) in values.iter_mut().zip(args) {
        match number(*arg) {
            Ok(x) => *slot = x,
            Err(err) => return err,
        }
    }
    let (x, y, z) = match function {
        "rgb_to_yiq" => rgb_to_yiq(values[0], values[1], values[2]),
        "yiq_to_rgb" => yiq_to_rgb(values[0], values[1], values[2]),
        "rgb_to_hls" => rgb_to_hls(values[0], values[1], values[2]),
        "hls_to_rgb" => hls_to_rgb(values[0], values[1], values[2]),
        "rgb_to_hsv" => rgb_to_hsv(values[0], values[1], values[2]),
        "hsv_to_rgb" => hsv_to_rgb(values[0], values[1], values[2]),
        _ => {
            return exc::raise_with_message(
                ATTRIBUTE_ERROR,
                &format!("module 'colorsys' has no attribute '{function}'"),
            )
        }
    };
    // Keep the components in tagged locals across the remaining allocations
    // so a collection cannot free them before the tuple owns them.
    let x = value::float_new(x);
    let y = value::float_new(y);
    let z = value::float_new(z);
    value::tuple_new(vec![x, y, z])
}

/// FCC NTSC luminance/chrominance coefficients.
fn rgb_to_yiq(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let y = 0.30 * r + 0.59 * g + 0.11 * b;
    let i = 0.74 * (r - y) - 0.27 * (b - y);
    let q = 0.48 * (r - y) + 0.41 * (b - y);
    (y, i, q)
}

fn yiq_to_rgb(y: f64, i: f64, q: f64) -> (f64, f64, f64) {
    let r = y + 0.9468822170900693 * i + 0.6235565819861433 * q;
    let g = y - 0.27478764629897834 * i - 0.6356910791873801 * q;
    let b = y - 1.1085450346420322 * i + 1.7090069284064666 * q;
    // `clamp` passes NaN through, matching CPython's chained comparisons.
    (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
}

fn rgb_to_hls(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let sumc = maxc + minc;
    let rangec = maxc - minc;
    let l = sumc / 2.0;
    if minc == maxc {
        return (0.0, l, 0.0);
    }
    let s = if l <= 0.5 {
        rangec / sumc
    } else {
        // Not always `2.0 - sumc`: gh-106498.
        rangec / (2.0 - maxc - minc)
    };
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    (mod_one(h / 6.0), l, s)
}

fn hls_to_rgb(h: f64, l: f64, s: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let m2 = if l <= 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let m1 = 2.0 * l - m2;
    (
        hue_to_rgb(m1, m2, h + ONE_THIRD),
        hue_to_rgb(m1, m2, h),
        hue_to_rgb(m1, m2, h - ONE_THIRD),
    )
}

fn hue_to_rgb(m1: f64, m2: f64, hue: f64) -> f64 {
    let hue = mod_one(hue);
    if hue < ONE_SIXTH {
        m1 + (m2 - m1) * hue * 6.0
    } else if hue < 0.5 {
        m2
    } else if hue < TWO_THIRD {
        m1 + (m2 - m1) * (TWO_THIRD - hue) * 6.0
    } else {
        m1
    }
}

fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let rangec = maxc - minc;
    let v = maxc;
    if minc == maxc {
        return (0.0, 0.0, v);
    }
    let s = rangec / maxc;
    let rc = (maxc - r) / rangec;
    let gc = (maxc - g) / rangec;
    let bc = (maxc - b) / rangec;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    (mod_one(h / 6.0), s, v)
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (v, v, v);
    }
    let scaled = h * 6.0;
    // `int()` truncates toward zero; the sector itself is taken modulo 6.
    let truncated = scaled as i64;
    let f = scaled - truncated as f64;
    let i = truncated.rem_euclid(6);
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::NONE;

    fn triple(v: i64) -> (f64, f64, f64) {
        let items = value::as_tuple(v).expect("tuple result");
        assert_eq!(items.len(), 3);
        (
            value::as_float(items[0]).expect("float component"),
            value::as_float(items[1]).expect("float component"),
            value::as_float(items[2]).expect("float component"),
        )
    }

    fn convert(function: &str, a: f64, b: f64, c: f64) -> (f64, f64, f64) {
        triple(call(
            function,
            &[
                value::float_new(a),
                value::float_new(b),
                value::float_new(c),
            ],
        ))
    }

    fn close(actual: (f64, f64, f64), expected: (f64, f64, f64)) {
        let pairs = [
            (actual.0, expected.0),
            (actual.1, expected.1),
            (actual.2, expected.2),
        ];
        for (a, e) in pairs {
            assert!(
                (a - e).abs() < 1e-12,
                "expected {expected:?}, got {actual:?}"
            );
        }
    }

    #[test]
    fn known_values() {
        exc::init();
        close(convert("rgb_to_yiq", 0.0, 0.0, 0.0), (0.0, 0.0, 0.0));
        close(convert("rgb_to_yiq", 1.0, 0.0, 0.0), (0.3, 0.599, 0.213));
        close(convert("rgb_to_hsv", 1.0, 0.0, 0.0), (0.0, 1.0, 1.0));
        close(convert("rgb_to_hsv", 0.0, 0.0, 1.0), (2.0 / 3.0, 1.0, 1.0));
        close(convert("rgb_to_hsv", 0.0, 0.0, 0.0), (0.0, 0.0, 0.0));
        close(convert("rgb_to_hls", 1.0, 0.0, 0.0), (0.0, 0.5, 1.0));
        close(convert("rgb_to_hls", 0.0, 0.0, 1.0), (2.0 / 3.0, 0.5, 1.0));
        close(convert("rgb_to_hls", 0.0, 0.0, 0.0), (0.0, 0.0, 0.0));
    }

    #[test]
    fn hls_near_white_uses_the_lightness_branch() {
        exc::init();
        let (h, l, s) = convert("rgb_to_hls", 1.0, 0.99, 1.0);
        close((h, l, s), (5.0 / 6.0, 0.995, 1.0));
        close(convert("hls_to_rgb", h, l, s), (1.0, 0.99, 1.0));
    }

    #[test]
    fn round_trips_recover_the_input() {
        exc::init();
        for color @ (r, g, b) in [
            (0.2, 0.4, 0.6),
            (0.1, 0.9, 0.3),
            (1.0, 0.99, 1.0),
            (0.0, 0.5, 1.0),
        ] {
            let (h, l, s) = convert("rgb_to_hls", r, g, b);
            close(convert("hls_to_rgb", h, l, s), color);
            let (h, s, v) = convert("rgb_to_hsv", r, g, b);
            close(convert("hsv_to_rgb", h, s, v), color);
            let (y, i, q) = convert("rgb_to_yiq", r, g, b);
            close(convert("yiq_to_rgb", y, i, q), color);
        }
        assert!(!exc::pending());
    }

    #[test]
    fn yiq_output_is_clamped() {
        exc::init();
        let (r, g, b) = convert("yiq_to_rgb", 0.0, 1.0, 1.0);
        assert!((0.0..=1.0).contains(&r));
        assert!((0.0..=1.0).contains(&g));
        assert!((0.0..=1.0).contains(&b));
    }

    #[test]
    fn non_numeric_components_raise_type_error() {
        exc::init();
        let result = call(
            "rgb_to_hsv",
            &[
                value::str_alloc("red".to_string()),
                value::float_new(0.0),
                value::float_new(0.0),
            ],
        );
        assert_eq!(result, NONE);
        assert_eq!(exc::pending_tag(), i64::from(TYPE_ERROR));
        assert_eq!(
            value::to_string(exc::pending_value()),
            "must be real number, not str"
        );
    }
}
