//! `cmath`: complex-number mathematical functions.
//!
//! The algorithms, branch cuts and special-value tables are a direct port of
//! CPython 3.14's `Modules/cmathmodule.c`. Each `c_*` helper returns the C99
//! Annex G value together with the `errno` it would set, and the dispatcher
//! turns `EDOM` into `ValueError: math domain error` and `ERANGE` into
//! `OverflowError: math range error`, exactly like the generated clinic
//! converters.
//!
//! Results are ordinary `complex` instances built by the runtime's complex
//! constructor, so `type(cmath.sqrt(-1)) is complex` holds. Non-numbers raise
//! `TypeError: must be real number, not ...`; a big integer too large for a
//! float raises `OverflowError`, as in CPython.

use std::sync::LazyLock;

use crate::builtins;
use crate::exc::{self, OVERFLOW_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::value;

const PI: f64 = std::f64::consts::PI;
/// C table spelling used by the ported special-value matrices.
const P: f64 = PI;
const E: f64 = std::f64::consts::E;
const TAU: f64 = std::f64::consts::TAU;
const LN2: f64 = std::f64::consts::LN_2;
const LN10: f64 = std::f64::consts::LN_10;
const INF: f64 = f64::INFINITY;
const NAN: f64 = f64::NAN;
/// Placeholder for table entries that are unreachable (both parts finite).
///
/// CPython's `-9.5426319407711027e33`, written as its exact bit pattern: the
/// decimal spelling needs more digits than `f64`'s guaranteed precision.
const U: f64 = f64::from_bits(0xc6fd_67cf_2c62_297c);

const P14: f64 = 0.25 * PI;
const P12: f64 = 0.5 * PI;
const P34: f64 = 0.75 * PI;

/// `DBL_MANT_DIG` on every supported target.
const DBL_MANT_DIG: i32 = 53;
const CM_SCALE_UP: i32 = 2 * (DBL_MANT_DIG / 2) + 1;
const CM_SCALE_DOWN: i32 = -(CM_SCALE_UP + 1) / 2;

static CM_LARGE_DOUBLE: LazyLock<f64> = LazyLock::new(|| f64::MAX / 4.0);
static CM_SQRT_LARGE_DOUBLE: LazyLock<f64> = LazyLock::new(|| (f64::MAX / 4.0).sqrt());
static CM_LOG_LARGE_DOUBLE: LazyLock<f64> = LazyLock::new(|| (f64::MAX / 4.0).ln());
static CM_SQRT_DBL_MIN: LazyLock<f64> = LazyLock::new(|| f64::MIN_POSITIVE.sqrt());

/// One complex value in C layout terms.
#[derive(Clone, Copy)]
struct C {
    re: f64,
    im: f64,
}

const fn c(re: f64, im: f64) -> C {
    C { re, im }
}

fn ldexp(x: f64, n: i32) -> f64 {
    x * 2f64.powi(n)
}

// -------------------------------------------------------------------------
// Special values
// -------------------------------------------------------------------------

const ST_NINF: usize = 0;
const ST_NEG: usize = 1;
const ST_NZERO: usize = 2;
const ST_PZERO: usize = 3;
const ST_POS: usize = 4;
const ST_PINF: usize = 5;
const ST_NAN: usize = 6;

fn special_type(d: f64) -> usize {
    if d.is_finite() {
        if d != 0.0 {
            if d.is_sign_positive() {
                ST_POS
            } else {
                ST_NEG
            }
        } else if d.is_sign_positive() {
            ST_PZERO
        } else {
            ST_NZERO
        }
    } else if d.is_nan() {
        ST_NAN
    } else if d.is_sign_positive() {
        ST_PINF
    } else {
        ST_NINF
    }
}

/// The `SPECIAL_VALUE` macro: table lookup when either part is non-finite.
fn special(z: C, table: &[[C; 7]; 7]) -> Option<C> {
    if z.re.is_finite() && z.im.is_finite() {
        None
    } else {
        Some(table[special_type(z.re)][special_type(z.im)])
    }
}

/// Final `errno` value of a C math routine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ErrNo {
    Ok,
    Domain,
    Range,
}

const ACOS_SPECIAL: [[C; 7]; 7] = [
    [
        c(P34, INF),
        c(P, INF),
        c(P, INF),
        c(P, -INF),
        c(P, -INF),
        c(P34, -INF),
        c(NAN, INF),
    ],
    [
        c(P12, INF),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(P12, -INF),
        c(NAN, NAN),
    ],
    [
        c(P12, INF),
        c(U, U),
        c(P12, 0.0),
        c(P12, -0.0),
        c(U, U),
        c(P12, -INF),
        c(P12, NAN),
    ],
    [
        c(P12, INF),
        c(U, U),
        c(P12, 0.0),
        c(P12, -0.0),
        c(U, U),
        c(P12, -INF),
        c(P12, NAN),
    ],
    [
        c(P12, INF),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(P12, -INF),
        c(NAN, NAN),
    ],
    [
        c(P14, INF),
        c(0.0, INF),
        c(0.0, INF),
        c(0.0, -INF),
        c(0.0, -INF),
        c(P14, -INF),
        c(NAN, INF),
    ],
    [
        c(NAN, INF),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, -INF),
        c(NAN, NAN),
    ],
];

const ACOSH_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, -P34),
        c(INF, -P),
        c(INF, -P),
        c(INF, P),
        c(INF, P),
        c(INF, P34),
        c(INF, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(0.0, -P12),
        c(0.0, P12),
        c(U, U),
        c(INF, P12),
        c(NAN, P12),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(0.0, -P12),
        c(0.0, P12),
        c(U, U),
        c(INF, P12),
        c(NAN, P12),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P14),
        c(INF, -0.0),
        c(INF, -0.0),
        c(INF, 0.0),
        c(INF, 0.0),
        c(INF, P14),
        c(INF, NAN),
    ],
    [
        c(INF, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(INF, NAN),
        c(NAN, NAN),
    ],
];

const ASINH_SPECIAL: [[C; 7]; 7] = [
    [
        c(-INF, -P14),
        c(-INF, -0.0),
        c(-INF, -0.0),
        c(-INF, 0.0),
        c(-INF, 0.0),
        c(-INF, P14),
        c(-INF, NAN),
    ],
    [
        c(-INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(-INF, P12),
        c(NAN, NAN),
    ],
    [
        c(-INF, -P12),
        c(U, U),
        c(-0.0, -0.0),
        c(-0.0, 0.0),
        c(U, U),
        c(-INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P14),
        c(INF, -0.0),
        c(INF, -0.0),
        c(INF, 0.0),
        c(INF, 0.0),
        c(INF, P14),
        c(INF, NAN),
    ],
    [
        c(INF, NAN),
        c(NAN, NAN),
        c(NAN, -0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(INF, NAN),
        c(NAN, NAN),
    ],
];

const ATANH_SPECIAL: [[C; 7]; 7] = [
    [
        c(-0.0, -P12),
        c(-0.0, -P12),
        c(-0.0, -P12),
        c(-0.0, P12),
        c(-0.0, P12),
        c(-0.0, P12),
        c(-0.0, NAN),
    ],
    [
        c(-0.0, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(-0.0, P12),
        c(NAN, NAN),
    ],
    [
        c(-0.0, -P12),
        c(U, U),
        c(-0.0, -0.0),
        c(-0.0, 0.0),
        c(U, U),
        c(-0.0, P12),
        c(-0.0, NAN),
    ],
    [
        c(0.0, -P12),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(0.0, P12),
        c(0.0, NAN),
    ],
    [
        c(0.0, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(0.0, P12),
        c(NAN, NAN),
    ],
    [
        c(0.0, -P12),
        c(0.0, -P12),
        c(0.0, -P12),
        c(0.0, P12),
        c(0.0, P12),
        c(0.0, P12),
        c(0.0, NAN),
    ],
    [
        c(0.0, -P12),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(0.0, P12),
        c(NAN, NAN),
    ],
];

const COSH_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, NAN),
        c(U, U),
        c(INF, 0.0),
        c(INF, -0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(NAN, 0.0),
        c(U, U),
        c(1.0, 0.0),
        c(1.0, -0.0),
        c(U, U),
        c(NAN, 0.0),
        c(NAN, 0.0),
    ],
    [
        c(NAN, 0.0),
        c(U, U),
        c(1.0, -0.0),
        c(1.0, 0.0),
        c(U, U),
        c(NAN, 0.0),
        c(NAN, 0.0),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(INF, NAN),
        c(U, U),
        c(INF, -0.0),
        c(INF, 0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, 0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
];

const EXP_SPECIAL: [[C; 7]; 7] = [
    [
        c(0.0, 0.0),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(0.0, 0.0),
        c(0.0, 0.0),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(1.0, -0.0),
        c(1.0, 0.0),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(1.0, -0.0),
        c(1.0, 0.0),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(INF, NAN),
        c(U, U),
        c(INF, -0.0),
        c(INF, 0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, -0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
];

const LOG_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, -P34),
        c(INF, -P),
        c(INF, -P),
        c(INF, P),
        c(INF, P),
        c(INF, P34),
        c(INF, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(-INF, -P),
        c(-INF, P),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(-INF, -0.0),
        c(-INF, 0.0),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P12),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, P12),
        c(NAN, NAN),
    ],
    [
        c(INF, -P14),
        c(INF, -0.0),
        c(INF, -0.0),
        c(INF, 0.0),
        c(INF, 0.0),
        c(INF, P14),
        c(INF, NAN),
    ],
    [
        c(INF, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(INF, NAN),
        c(NAN, NAN),
    ],
];

const SINH_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, NAN),
        c(U, U),
        c(-INF, -0.0),
        c(-INF, 0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(0.0, NAN),
        c(U, U),
        c(-0.0, -0.0),
        c(-0.0, 0.0),
        c(U, U),
        c(0.0, NAN),
        c(0.0, NAN),
    ],
    [
        c(0.0, NAN),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(0.0, NAN),
        c(0.0, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(INF, NAN),
        c(U, U),
        c(INF, -0.0),
        c(INF, 0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, -0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
];

const SQRT_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, -INF),
        c(0.0, -INF),
        c(0.0, -INF),
        c(0.0, INF),
        c(0.0, INF),
        c(INF, INF),
        c(NAN, INF),
    ],
    [
        c(INF, -INF),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, INF),
        c(NAN, NAN),
    ],
    [
        c(INF, -INF),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(INF, INF),
        c(NAN, NAN),
    ],
    [
        c(INF, -INF),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(INF, INF),
        c(NAN, NAN),
    ],
    [
        c(INF, -INF),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(INF, INF),
        c(NAN, NAN),
    ],
    [
        c(INF, -INF),
        c(INF, -0.0),
        c(INF, -0.0),
        c(INF, 0.0),
        c(INF, 0.0),
        c(INF, INF),
        c(INF, NAN),
    ],
    [
        c(INF, -INF),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
        c(INF, INF),
        c(NAN, NAN),
    ],
];

const TANH_SPECIAL: [[C; 7]; 7] = [
    [
        c(-1.0, 0.0),
        c(U, U),
        c(-1.0, -0.0),
        c(-1.0, 0.0),
        c(U, U),
        c(-1.0, 0.0),
        c(-1.0, 0.0),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(-0.0, NAN),
        c(U, U),
        c(-0.0, -0.0),
        c(-0.0, 0.0),
        c(U, U),
        c(-0.0, NAN),
        c(-0.0, NAN),
    ],
    [
        c(0.0, NAN),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(0.0, NAN),
        c(0.0, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(1.0, 0.0),
        c(U, U),
        c(1.0, -0.0),
        c(1.0, 0.0),
        c(U, U),
        c(1.0, 0.0),
        c(1.0, 0.0),
    ],
    [
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, -0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
];

const RECT_SPECIAL: [[C; 7]; 7] = [
    [
        c(INF, NAN),
        c(U, U),
        c(-INF, 0.0),
        c(-INF, -0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(0.0, 0.0),
        c(U, U),
        c(-0.0, 0.0),
        c(-0.0, -0.0),
        c(U, U),
        c(0.0, 0.0),
        c(0.0, 0.0),
    ],
    [
        c(0.0, 0.0),
        c(U, U),
        c(0.0, -0.0),
        c(0.0, 0.0),
        c(U, U),
        c(0.0, 0.0),
        c(0.0, 0.0),
    ],
    [
        c(NAN, NAN),
        c(U, U),
        c(U, U),
        c(U, U),
        c(U, U),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
    [
        c(INF, NAN),
        c(U, U),
        c(INF, -0.0),
        c(INF, 0.0),
        c(U, U),
        c(INF, NAN),
        c(INF, NAN),
    ],
    [
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, 0.0),
        c(NAN, 0.0),
        c(NAN, NAN),
        c(NAN, NAN),
        c(NAN, NAN),
    ],
];

// -------------------------------------------------------------------------
// Complex primitives
// -------------------------------------------------------------------------

fn c_neg(z: C) -> C {
    c(-z.re, -z.im)
}

fn c_diff(a: C, b: C) -> C {
    c(a.re - b.re, a.im - b.im)
}

/// `_Py_c_abs`: C99 rules for infinite and NaN parts.
fn c_abs(z: C) -> (f64, ErrNo) {
    if !z.re.is_finite() || !z.im.is_finite() {
        if z.re.is_infinite() {
            return (z.re.abs(), ErrNo::Ok);
        }
        if z.im.is_infinite() {
            return (z.im.abs(), ErrNo::Ok);
        }
        return (NAN, ErrNo::Ok);
    }
    let result = z.re.hypot(z.im);
    if result.is_finite() {
        (result, ErrNo::Ok)
    } else {
        (result, ErrNo::Range)
    }
}

/// `_Py_c_quot`, including the C11 Annex G nan recovery.
fn c_quot(a: C, b: C) -> (C, ErrNo) {
    let abs_bre = b.re.abs();
    let abs_bim = b.im.abs();
    let mut errno = ErrNo::Ok;
    let mut r = if abs_bre >= abs_bim {
        if abs_bre == 0.0 {
            errno = ErrNo::Domain;
            c(0.0, 0.0)
        } else {
            let ratio = b.im / b.re;
            let denom = b.re + b.im * ratio;
            c((a.re + a.im * ratio) / denom, (a.im - a.re * ratio) / denom)
        }
    } else if abs_bim >= abs_bre {
        let ratio = b.re / b.im;
        let denom = b.re * ratio + b.im;
        c((a.re * ratio + a.im) / denom, (a.im * ratio - a.re) / denom)
    } else {
        c(NAN, NAN)
    };
    if r.re.is_nan() && r.im.is_nan() {
        if (a.re.is_infinite() || a.im.is_infinite()) && b.re.is_finite() && b.im.is_finite() {
            let x = f64::copysign(if a.re.is_infinite() { 1.0 } else { 0.0 }, a.re);
            let y = f64::copysign(if a.im.is_infinite() { 1.0 } else { 0.0 }, a.im);
            r.re = INF * (x * b.re + y * b.im);
            r.im = INF * (y * b.re - x * b.im);
        } else if (abs_bre.is_infinite() || abs_bim.is_infinite())
            && a.re.is_finite()
            && a.im.is_finite()
        {
            let x = f64::copysign(if b.re.is_infinite() { 1.0 } else { 0.0 }, b.re);
            let y = f64::copysign(if b.im.is_infinite() { 1.0 } else { 0.0 }, b.im);
            r.re = 0.0 * (a.re * x + a.im * y);
            r.im = 0.0 * (a.im * x - a.re * y);
        }
    }
    (r, errno)
}

// -------------------------------------------------------------------------
// C math routines
// -------------------------------------------------------------------------

fn sqrt_impl(z: C) -> C {
    if let Some(result) = special(z, &SQRT_SPECIAL) {
        return result;
    }
    if z.re == 0.0 && z.im == 0.0 {
        return c(0.0, z.im);
    }
    let mut ax = z.re.abs();
    let ay = z.im.abs();

    let s = if ax < f64::MIN_POSITIVE && ay < f64::MIN_POSITIVE {
        ax = ldexp(ax, CM_SCALE_UP);
        ldexp(
            (ax + ax.hypot(ldexp(ay, CM_SCALE_UP))).sqrt(),
            CM_SCALE_DOWN,
        )
    } else {
        ax /= 8.0;
        2.0 * (ax + ax.hypot(ay / 8.0)).sqrt()
    };
    let d = ay / (2.0 * s);
    if z.re >= 0.0 {
        c(s, f64::copysign(d, z.im))
    } else {
        c(d, f64::copysign(s, z.im))
    }
}

fn acos_impl(z: C) -> C {
    if let Some(result) = special(z, &ACOS_SPECIAL) {
        return result;
    }
    if z.re.abs() > *CM_LARGE_DOUBLE || z.im.abs() > *CM_LARGE_DOUBLE {
        let re = z.im.abs().atan2(z.re);
        let im = -f64::copysign((z.re / 2.0).hypot(z.im / 2.0).ln() + LN2 * 2.0, z.im);
        return c(re, im);
    }
    let s1 = sqrt_impl(c(1.0 - z.re, -z.im));
    let s2 = sqrt_impl(c(1.0 + z.re, z.im));
    c(
        2.0 * s1.re.atan2(s2.re),
        (s2.re * s1.im - s2.im * s1.re).asinh(),
    )
}

fn acosh_impl(z: C) -> C {
    if let Some(result) = special(z, &ACOSH_SPECIAL) {
        return result;
    }
    if z.re.abs() > *CM_LARGE_DOUBLE || z.im.abs() > *CM_LARGE_DOUBLE {
        return c(
            (z.re / 2.0).hypot(z.im / 2.0).ln() + LN2 * 2.0,
            z.im.atan2(z.re),
        );
    }
    let s1 = sqrt_impl(c(z.re - 1.0, z.im));
    let s2 = sqrt_impl(c(z.re + 1.0, z.im));
    c(
        (s1.re * s2.re + s1.im * s2.im).asinh(),
        2.0 * s1.im.atan2(s2.re),
    )
}

fn asinh_impl(z: C) -> C {
    if let Some(result) = special(z, &ASINH_SPECIAL) {
        return result;
    }
    if z.re.abs() > *CM_LARGE_DOUBLE || z.im.abs() > *CM_LARGE_DOUBLE {
        let magnitude = (z.re / 2.0).hypot(z.im / 2.0).ln() + LN2 * 2.0;
        let re = if z.im >= 0.0 {
            f64::copysign(magnitude, z.re)
        } else {
            -f64::copysign(magnitude, -z.re)
        };
        return c(re, z.im.atan2(z.re.abs()));
    }
    let s1 = sqrt_impl(c(1.0 + z.im, -z.re));
    let s2 = sqrt_impl(c(1.0 - z.im, z.re));
    c(
        (s1.re * s2.im - s2.re * s1.im).asinh(),
        z.im.atan2(s1.re * s2.re - s1.im * s2.im),
    )
}

fn asin_impl(z: C) -> C {
    let s = asinh_impl(c(-z.im, z.re));
    c(s.im, -s.re)
}

fn atanh_impl(z: C) -> (C, ErrNo) {
    if let Some(result) = special(z, &ATANH_SPECIAL) {
        return (result, ErrNo::Ok);
    }
    if z.re < 0.0 {
        let (negated, errno) = atanh_impl(c_neg(z));
        return (c_neg(negated), errno);
    }
    let ay = z.im.abs();
    if z.re > *CM_SQRT_LARGE_DOUBLE || ay > *CM_SQRT_LARGE_DOUBLE {
        let h = (z.re / 2.0).hypot(z.im / 2.0);
        return (c(z.re / 4.0 / h / h, f64::copysign(P12, z.im)), ErrNo::Ok);
    }
    if z.re == 1.0 && ay < *CM_SQRT_DBL_MIN {
        if ay == 0.0 {
            return (c(INF, z.im), ErrNo::Domain);
        }
        let re = -(ay.sqrt() / ay.hypot(2.0).sqrt()).ln();
        let im = f64::copysign(2.0f64.atan2(-ay) / 2.0, z.im);
        return (c(re, im), ErrNo::Ok);
    }
    let re = (4.0 * z.re / ((1.0 - z.re) * (1.0 - z.re) + ay * ay)).ln_1p() / 4.0;
    let im = -(-2.0 * z.im).atan2((1.0 - z.re) * (1.0 + z.re) - ay * ay) / 2.0;
    (c(re, im), ErrNo::Ok)
}

fn atan_impl(z: C) -> (C, ErrNo) {
    let (s, errno) = atanh_impl(c(-z.im, z.re));
    (c(s.im, -s.re), errno)
}

fn cosh_impl(z: C) -> (C, ErrNo) {
    if !z.re.is_finite() || !z.im.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                c(
                    f64::copysign(INF, z.im.cos()),
                    f64::copysign(INF, z.im.sin()),
                )
            } else {
                c(
                    f64::copysign(INF, z.im.cos()),
                    -f64::copysign(INF, z.im.sin()),
                )
            }
        } else {
            COSH_SPECIAL[special_type(z.re)][special_type(z.im)]
        };
        let errno = if z.im.is_infinite() && !z.re.is_nan() {
            ErrNo::Domain
        } else {
            ErrNo::Ok
        };
        return (r, errno);
    }
    let r = if z.re.abs() > *CM_LOG_LARGE_DOUBLE {
        let x_minus_one = z.re - f64::copysign(1.0, z.re);
        c(
            z.im.cos() * x_minus_one.cosh() * E,
            z.im.sin() * x_minus_one.sinh() * E,
        )
    } else {
        c(z.im.cos() * z.re.cosh(), z.im.sin() * z.re.sinh())
    };
    let errno = if r.re.is_infinite() || r.im.is_infinite() {
        ErrNo::Range
    } else {
        ErrNo::Ok
    };
    (r, errno)
}

fn sinh_impl(z: C) -> (C, ErrNo) {
    if !z.re.is_finite() || !z.im.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                c(
                    f64::copysign(INF, z.im.cos()),
                    f64::copysign(INF, z.im.sin()),
                )
            } else {
                c(
                    -f64::copysign(INF, z.im.cos()),
                    f64::copysign(INF, z.im.sin()),
                )
            }
        } else {
            SINH_SPECIAL[special_type(z.re)][special_type(z.im)]
        };
        let errno = if z.im.is_infinite() && !z.re.is_nan() {
            ErrNo::Domain
        } else {
            ErrNo::Ok
        };
        return (r, errno);
    }
    let r = if z.re.abs() > *CM_LOG_LARGE_DOUBLE {
        let x_minus_one = z.re - f64::copysign(1.0, z.re);
        c(
            z.im.cos() * x_minus_one.sinh() * E,
            z.im.sin() * x_minus_one.cosh() * E,
        )
    } else {
        c(z.im.cos() * z.re.sinh(), z.im.sin() * z.re.cosh())
    };
    let errno = if r.re.is_infinite() || r.im.is_infinite() {
        ErrNo::Range
    } else {
        ErrNo::Ok
    };
    (r, errno)
}

fn cos_impl(z: C) -> (C, ErrNo) {
    cosh_impl(c(-z.im, z.re))
}

fn sin_impl(z: C) -> (C, ErrNo) {
    let (s, errno) = sinh_impl(c(-z.im, z.re));
    (c(s.im, -s.re), errno)
}

fn tanh_impl(z: C) -> (C, ErrNo) {
    if !z.re.is_finite() || !z.im.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            let sign = if z.re > 0.0 { 1.0 } else { -1.0 };
            c(sign, f64::copysign(0.0, 2.0 * z.im.sin() * z.im.cos()))
        } else {
            TANH_SPECIAL[special_type(z.re)][special_type(z.im)]
        };
        let errno = if z.im.is_infinite() && z.re.is_finite() {
            ErrNo::Domain
        } else {
            ErrNo::Ok
        };
        return (r, errno);
    }
    let r = if z.re.abs() > *CM_LOG_LARGE_DOUBLE {
        c(
            f64::copysign(1.0, z.re),
            4.0 * z.im.sin() * z.im.cos() * (-2.0 * z.re.abs()).exp(),
        )
    } else {
        let tx = z.re.tanh();
        let ty = z.im.tan();
        let cx = 1.0 / z.re.cosh();
        let txty = tx * ty;
        let denom = 1.0 + txty * txty;
        c(tx * (1.0 + ty * ty) / denom, ((ty / denom) * cx) * cx)
    };
    (r, ErrNo::Ok)
}

fn tan_impl(z: C) -> (C, ErrNo) {
    let (s, errno) = tanh_impl(c(-z.im, z.re));
    (c(s.im, -s.re), errno)
}

fn exp_impl(z: C) -> (C, ErrNo) {
    if !z.re.is_finite() || !z.im.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                c(
                    f64::copysign(INF, z.im.cos()),
                    f64::copysign(INF, z.im.sin()),
                )
            } else {
                c(
                    f64::copysign(0.0, z.im.cos()),
                    f64::copysign(0.0, z.im.sin()),
                )
            }
        } else {
            EXP_SPECIAL[special_type(z.re)][special_type(z.im)]
        };
        let errno =
            if z.im.is_infinite() && (z.re.is_finite() || (z.re.is_infinite() && z.re > 0.0)) {
                ErrNo::Domain
            } else {
                ErrNo::Ok
            };
        return (r, errno);
    }
    let r = if z.re > *CM_LOG_LARGE_DOUBLE {
        let l = (z.re - 1.0).exp();
        c(l * z.im.cos() * E, l * z.im.sin() * E)
    } else {
        let l = z.re.exp();
        c(l * z.im.cos(), l * z.im.sin())
    };
    let errno = if r.re.is_infinite() || r.im.is_infinite() {
        ErrNo::Range
    } else {
        ErrNo::Ok
    };
    (r, errno)
}

fn log_impl(z: C) -> (C, ErrNo) {
    if let Some(result) = special(z, &LOG_SPECIAL) {
        return (result, ErrNo::Ok);
    }
    let ax = z.re.abs();
    let ay = z.im.abs();
    let re = if ax > *CM_LARGE_DOUBLE || ay > *CM_LARGE_DOUBLE {
        (ax / 2.0).hypot(ay / 2.0).ln() + LN2
    } else if ax < f64::MIN_POSITIVE && ay < f64::MIN_POSITIVE {
        if ax > 0.0 || ay > 0.0 {
            ldexp(ax, DBL_MANT_DIG).hypot(ldexp(ay, DBL_MANT_DIG)).ln() - DBL_MANT_DIG as f64 * LN2
        } else {
            return (c(-INF, z.im.atan2(z.re)), ErrNo::Domain);
        }
    } else {
        let h = ax.hypot(ay);
        if (0.71..=1.73).contains(&h) {
            let am = ax.max(ay);
            let an = ax.min(ay);
            ((am - 1.0) * (am + 1.0) + an * an).ln_1p() / 2.0
        } else {
            h.ln()
        }
    };
    (c(re, z.im.atan2(z.re)), ErrNo::Ok)
}

fn log10_impl(z: C) -> (C, ErrNo) {
    let (r, errno) = log_impl(z);
    (c(r.re / LN10, r.im / LN10), errno)
}

fn rect_impl(r: f64, phi: f64) -> (C, ErrNo) {
    if !r.is_finite() || !phi.is_finite() {
        let z = if r.is_infinite() && phi.is_finite() && phi != 0.0 {
            if r > 0.0 {
                c(f64::copysign(INF, phi.cos()), f64::copysign(INF, phi.sin()))
            } else {
                c(
                    -f64::copysign(INF, phi.cos()),
                    -f64::copysign(INF, phi.sin()),
                )
            }
        } else {
            RECT_SPECIAL[special_type(r)][special_type(phi)]
        };
        let errno = if r != 0.0 && !r.is_nan() && phi.is_infinite() {
            ErrNo::Domain
        } else {
            ErrNo::Ok
        };
        return (z, errno);
    }
    if phi == 0.0 {
        return (c(r, r * phi), ErrNo::Ok);
    }
    (c(r * phi.cos(), r * phi.sin()), ErrNo::Ok)
}

// -------------------------------------------------------------------------
// Argument conversion and results
// -------------------------------------------------------------------------

fn number(v: i64) -> Result<f64, i64> {
    if let Some(f) = value::as_float(v) {
        return Ok(f);
    }
    if let Some(i) = value::as_integer(v) {
        return Ok(i as f64);
    }
    if let Some(n) = value::as_bigint(v) {
        return value::bigint_to_f64(n).ok_or_else(|| {
            exc::raise_with_message(OVERFLOW_ERROR, "int too large to convert to float")
        });
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("must be real number, not {}", value::type_name_of(v)),
    ))
}

fn complex_arg(v: i64) -> Result<C, i64> {
    if let Some((re, im)) = builtins::complex_components(v) {
        return Ok(c(re, im));
    }
    if let Some(n) = value::as_bigint(v) {
        return match value::bigint_to_f64(n) {
            Some(f) => Ok(c(f, 0.0)),
            None => Err(exc::raise_with_message(
                OVERFLOW_ERROR,
                "int too large to convert to float",
            )),
        };
    }
    Err(exc::raise_with_message(
        TYPE_ERROR,
        &format!("must be real number, not {}", value::type_name_of(v)),
    ))
}

fn complex_result(result: (C, ErrNo)) -> i64 {
    match result.1 {
        ErrNo::Ok => builtins::build_complex(result.0.re, result.0.im),
        ErrNo::Domain => exc::raise_with_message(VALUE_ERROR, "math domain error"),
        ErrNo::Range => exc::raise_with_message(OVERFLOW_ERROR, "math range error"),
    }
}

fn complex_value(result: C) -> i64 {
    builtins::build_complex(result.re, result.im)
}

fn one_arg(args: &[i64], name: &str) -> Result<C, i64> {
    if args.len() != 1 {
        return Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "cmath.{name}() takes exactly one argument ({} given)",
                args.len()
            ),
        ));
    }
    complex_arg(args[0])
}

fn log_call(args: &[i64]) -> i64 {
    if args.is_empty() {
        return exc::raise_with_message(TYPE_ERROR, "log expected at least 1 argument, got 0");
    }
    if args.len() > 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("log expected at most 2 arguments, got {}", args.len()),
        );
    }
    let z = match complex_arg(args[0]) {
        Ok(z) => z,
        Err(error) => return error,
    };
    let (x, errno) = log_impl(z);
    if args.len() == 2 {
        let base = match complex_arg(args[1]) {
            Ok(base) => base,
            Err(error) => return error,
        };
        // The second `c_log` resets `errno`; only the quotient can raise.
        let (b, _) = log_impl(base);
        let (quotient, quotient_errno) = c_quot(x, b);
        return complex_result((quotient, quotient_errno));
    }
    complex_result((x, errno))
}

fn rect_call(args: &[i64]) -> i64 {
    if args.len() != 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("rect expected 2 arguments, got {}", args.len()),
        );
    }
    let r = match number(args[0]) {
        Ok(r) => r,
        Err(error) => return error,
    };
    let phi = match number(args[1]) {
        Ok(phi) => phi,
        Err(error) => return error,
    };
    complex_result(rect_impl(r, phi))
}

fn isclose_values(a: C, b: C, rel_tol: f64, abs_tol: f64) -> Result<bool, i64> {
    if rel_tol < 0.0 || abs_tol < 0.0 {
        return Err(exc::raise_with_message(
            VALUE_ERROR,
            "tolerances must be non-negative",
        ));
    }
    if a.re == b.re && a.im == b.im {
        return Ok(true);
    }
    if a.re.is_infinite() || a.im.is_infinite() || b.re.is_infinite() || b.im.is_infinite() {
        return Ok(false);
    }
    let diff = c_abs(c_diff(a, b)).0;
    Ok(diff <= rel_tol * c_abs(b).0 || diff <= rel_tol * c_abs(a).0 || diff <= abs_tol)
}

fn isclose_with(a: i64, b: i64, rel_tol: Option<i64>, abs_tol: Option<i64>) -> i64 {
    let a = match complex_arg(a) {
        Ok(a) => a,
        Err(error) => return error,
    };
    let b = match complex_arg(b) {
        Ok(b) => b,
        Err(error) => return error,
    };
    let rel_tol = match rel_tol {
        Some(value) => match number(value) {
            Ok(value) => value,
            Err(error) => return error,
        },
        None => 1e-9,
    };
    let abs_tol = match abs_tol {
        Some(value) => match number(value) {
            Ok(value) => value,
            Err(error) => return error,
        },
        None => 0.0,
    };
    match isclose_values(a, b, rel_tol, abs_tol) {
        Ok(true) => value::TRUE,
        Ok(false) => value::FALSE,
        Err(error) => error,
    }
}

fn isclose_call(args: &[i64]) -> i64 {
    if args.len() < 2 {
        return exc::raise_with_message(
            TYPE_ERROR,
            "isclose() missing required argument 'b' (pos 2)",
        );
    }
    if args.len() > 4 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("isclose() takes at most 4 arguments ({} given)", args.len()),
        );
    }
    isclose_with(args[0], args[1], args.get(2).copied(), args.get(3).copied())
}

/// Keyword-aware dispatch for `isclose(a, b, *, rel_tol=1e-09, abs_tol=0.0)`.
pub(crate) fn call_kw(function: &str, args: &[i64], keywords: &[(String, i64)]) -> i64 {
    if function != "isclose" {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("cmath.{function}() does not accept keyword arguments"),
        );
    }
    if args.len() > 4 {
        return exc::raise_with_message(
            TYPE_ERROR,
            &format!("isclose() takes at most 4 arguments ({} given)", args.len()),
        );
    }
    let mut slots: [Option<i64>; 4] = [None; 4];
    for (index, value) in args.iter().enumerate() {
        slots[index] = Some(*value);
    }
    for (name, value) in keywords {
        let index = match name.as_str() {
            "a" => 0,
            "b" => 1,
            "rel_tol" => 2,
            "abs_tol" => 3,
            _ => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!("isclose() got an unexpected keyword argument '{name}'"),
                )
            }
        };
        if slots[index].is_some() {
            return exc::raise_with_message(
                TYPE_ERROR,
                &format!("isclose() got multiple values for argument '{name}'"),
            );
        }
        slots[index] = Some(*value);
    }
    let Some(a) = slots[0] else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "isclose() missing required argument 'a' (pos 1)",
        );
    };
    let Some(b) = slots[1] else {
        return exc::raise_with_message(
            TYPE_ERROR,
            "isclose() missing required argument 'b' (pos 2)",
        );
    };
    isclose_with(a, b, slots[2], slots[3])
}

// -------------------------------------------------------------------------
// Dispatch
// -------------------------------------------------------------------------

/// Dispatches one `cmath` module function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "acos" => match one_arg(args, "acos") {
            Ok(z) => complex_value(acos_impl(z)),
            Err(error) => error,
        },
        "acosh" => match one_arg(args, "acosh") {
            Ok(z) => complex_value(acosh_impl(z)),
            Err(error) => error,
        },
        "asin" => match one_arg(args, "asin") {
            Ok(z) => complex_value(asin_impl(z)),
            Err(error) => error,
        },
        "asinh" => match one_arg(args, "asinh") {
            Ok(z) => complex_value(asinh_impl(z)),
            Err(error) => error,
        },
        "atan" => match one_arg(args, "atan") {
            Ok(z) => complex_result(atan_impl(z)),
            Err(error) => error,
        },
        "atanh" => match one_arg(args, "atanh") {
            Ok(z) => complex_result(atanh_impl(z)),
            Err(error) => error,
        },
        "cos" => match one_arg(args, "cos") {
            Ok(z) => complex_result(cos_impl(z)),
            Err(error) => error,
        },
        "cosh" => match one_arg(args, "cosh") {
            Ok(z) => complex_result(cosh_impl(z)),
            Err(error) => error,
        },
        "exp" => match one_arg(args, "exp") {
            Ok(z) => complex_result(exp_impl(z)),
            Err(error) => error,
        },
        "log" => log_call(args),
        "log10" => match one_arg(args, "log10") {
            Ok(z) => complex_result(log10_impl(z)),
            Err(error) => error,
        },
        "sin" => match one_arg(args, "sin") {
            Ok(z) => complex_result(sin_impl(z)),
            Err(error) => error,
        },
        "sinh" => match one_arg(args, "sinh") {
            Ok(z) => complex_result(sinh_impl(z)),
            Err(error) => error,
        },
        "sqrt" => match one_arg(args, "sqrt") {
            Ok(z) => complex_value(sqrt_impl(z)),
            Err(error) => error,
        },
        "tan" => match one_arg(args, "tan") {
            Ok(z) => complex_result(tan_impl(z)),
            Err(error) => error,
        },
        "tanh" => match one_arg(args, "tanh") {
            Ok(z) => complex_result(tanh_impl(z)),
            Err(error) => error,
        },
        "phase" => match one_arg(args, "phase") {
            Ok(z) => value::float_new(z.im.atan2(z.re)),
            Err(error) => error,
        },
        "polar" => match one_arg(args, "polar") {
            Ok(z) => {
                let phi = z.im.atan2(z.re);
                let (r, errno) = c_abs(z);
                if errno == ErrNo::Range {
                    exc::raise_with_message(OVERFLOW_ERROR, "math range error")
                } else {
                    let r = value::float_new(r);
                    let phi = value::float_new(phi);
                    value::tuple_new(vec![r, phi])
                }
            }
            Err(error) => error,
        },
        "rect" => rect_call(args),
        "isfinite" => match one_arg(args, "isfinite") {
            Ok(z) => value::bool_value(z.re.is_finite() && z.im.is_finite()),
            Err(error) => error,
        },
        "isinf" => match one_arg(args, "isinf") {
            Ok(z) => value::bool_value(z.re.is_infinite() || z.im.is_infinite()),
            Err(error) => error,
        },
        "isnan" => match one_arg(args, "isnan") {
            Ok(z) => value::bool_value(z.re.is_nan() || z.im.is_nan()),
            Err(error) => error,
        },
        "isclose" => isclose_call(args),
        _ => exc::raise_with_message(
            exc::ATTRIBUTE_ERROR,
            &format!("module 'cmath' has no attribute '{function}'"),
        ),
    }
}

/// Module constants (`pi`, `e`, `tau`, `inf`, `infj`, `nan`, `nanj`).
pub(crate) fn constant(name: &str) -> Option<i64> {
    match name {
        "pi" => Some(value::float_new(PI)),
        "e" => Some(value::float_new(E)),
        "tau" => Some(value::float_new(TAU)),
        "inf" => Some(value::float_new(INF)),
        "nan" => Some(value::float_new(f64::NAN.abs())),
        "infj" => Some(builtins::build_complex(0.0, INF)),
        "nanj" => Some(builtins::build_complex(0.0, f64::NAN.abs())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(re: f64, im: f64) -> i64 {
        builtins::build_complex(re, im)
    }

    fn parts(v: i64) -> (f64, f64) {
        builtins::complex_components(v).expect("complex result")
    }

    fn real(v: i64) -> f64 {
        value::as_float(v).expect("float result")
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    fn call_complex(name: &str, re: f64, im: f64) -> (f64, f64) {
        parts(call(name, &[comp(re, im)]))
    }

    fn take_error(tag: u32) -> String {
        assert_eq!(
            exc::pending_tag(),
            i64::from(tag),
            "unexpected pending exception"
        );
        let message = value::to_string(exc::pending_value());
        exc::clear();
        message
    }

    #[test]
    fn known_values() {
        exc::init();
        let (re, im) = call_complex("sqrt", -1.0, 0.0);
        assert!(close(re, 0.0) && close(im, 1.0));
        let (re, im) = call_complex("exp", 0.0, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("log", E, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("log10", 100.0, 0.0);
        assert!(close(re, 2.0) && close(im, 0.0));
        let (re, im) = call_complex("sin", PI / 2.0, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("cos", 0.0, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("tan", PI / 4.0, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("asin", 1.0, 0.0);
        assert!(close(re, PI / 2.0) && close(im, 0.0));
        let (re, im) = call_complex("acos", 0.0, 0.0);
        assert!(close(re, PI / 2.0) && close(im, 0.0));
        let (re, im) = call_complex("atan", 1.0, 0.0);
        assert!(close(re, PI / 4.0) && close(im, 0.0));
        let (re, im) = call_complex("sinh", 0.0, 0.0);
        assert!(close(re, 0.0) && close(im, 0.0));
        let (re, im) = call_complex("cosh", 0.0, 0.0);
        assert!(close(re, 1.0) && close(im, 0.0));
        let (re, im) = call_complex("tanh", 0.0, 0.0);
        assert!(close(re, 0.0) && close(im, 0.0));
        let (re, im) = call_complex("asinh", 0.0, 0.0);
        assert!(close(re, 0.0) && close(im, 0.0));
        let (re, im) = call_complex("acosh", 1.0, 0.0);
        assert!(close(re, 0.0) && close(im, 0.0));
        let (re, im) = call_complex("atanh", 0.0, 0.0);
        assert!(close(re, 0.0) && close(im, 0.0));
    }

    #[test]
    fn branch_cuts() {
        exc::init();
        // log(-1) = i*pi.
        let (re, im) = call_complex("log", -1.0, 0.0);
        assert!(close(re, 0.0) && close(im, PI));
        // sqrt(-4) = 2i.
        let (re, im) = call_complex("sqrt", -4.0, 0.0);
        assert!(close(re, 0.0) && close(im, 2.0));
        // acos(2) = -i*acosh(2).
        let (re, im) = call_complex("acos", 2.0, 0.0);
        assert!(close(re, 0.0) && close(im, -(2.0f64 + 3.0f64.sqrt()).ln()));
        // asin(2) = pi/2 + i*ln(2+sqrt(3)).
        let (re, im) = call_complex("asin", 2.0, 0.0);
        assert!(close(re, PI / 2.0) && close(im, (2.0f64 + 3.0f64.sqrt()).ln()));
        // atanh(2) = 0.5*ln(3) + i*pi/2.
        let (re, im) = call_complex("atanh", 2.0, 0.0);
        assert!(close(re, 3.0f64.ln() / 2.0) && close(im, PI / 2.0));
        // atan(1j) is a domain error, like CPython.
        call("atan", &[comp(0.0, 1.0)]);
        assert_eq!(take_error(VALUE_ERROR), "math domain error");
    }

    #[test]
    fn special_values() {
        exc::init();
        assert_eq!(call("isfinite", &[comp(INF, 0.0)]), value::FALSE);
        assert_eq!(call("isinf", &[comp(INF, 0.0)]), value::TRUE);
        assert_eq!(call("isnan", &[comp(NAN, 0.0)]), value::TRUE);
        assert_eq!(call("isfinite", &[comp(1.0, 2.0)]), value::TRUE);
        // sqrt(inf+infj) = inf+infj.
        let (re, im) = call_complex("sqrt", INF, INF);
        assert!(re.is_infinite() && im.is_infinite());
        // acos(inf+1j) = -infj.
        let (re, im) = call_complex("acos", INF, 1.0);
        assert!(re == 0.0 && im.is_infinite() && im < 0.0);
        // cos(inf+1j) is a domain error.
        call("cos", &[comp(INF, 1.0)]);
        assert_eq!(take_error(VALUE_ERROR), "math domain error");
        // polar(0) = (0, 0) and rect(0, inf) = 0.
        let polar = call("polar", &[comp(0.0, 0.0)]);
        let items = value::as_tuple(polar).expect("tuple");
        assert_eq!(real(items[0]), 0.0);
        assert_eq!(real(items[1]), 0.0);
        let (re, im) = parts(call(
            "rect",
            &[value::float_new(0.0), value::float_new(INF)],
        ));
        assert!(re == 0.0 && im == 0.0);
    }

    #[test]
    fn errors() {
        exc::init();
        call("log", &[comp(0.0, 0.0)]);
        assert_eq!(take_error(VALUE_ERROR), "math domain error");
        call("atanh", &[comp(1.0, 0.0)]);
        assert_eq!(take_error(VALUE_ERROR), "math domain error");
        call("exp", &[comp(1000.0, 0.0)]);
        assert_eq!(take_error(OVERFLOW_ERROR), "math range error");
        call("rect", &[value::float_new(1.0), value::float_new(INF)]);
        assert_eq!(take_error(VALUE_ERROR), "math domain error");
        call("sqrt", &[value::str_alloc("x".to_string())]);
        assert_eq!(take_error(TYPE_ERROR), "must be real number, not str");
        let huge = value::bigint_new(num_bigint::BigInt::from(10).pow(400));
        call("sqrt", &[huge]);
        assert_eq!(
            take_error(OVERFLOW_ERROR),
            "int too large to convert to float"
        );
        call(
            "isclose",
            &[
                value::float_new(1.0),
                value::float_new(1.0),
                value::float_new(-1.0),
            ],
        );
        assert_eq!(take_error(VALUE_ERROR), "tolerances must be non-negative");
    }

    #[test]
    fn log_with_base_and_isclose() {
        exc::init();
        let (re, im) = parts(call(
            "log",
            &[
                value::encode_int(8).expect("int"),
                value::encode_int(2).expect("int"),
            ],
        ));
        assert!(close(re, 3.0) && close(im, 0.0));
        // log(0, 2) is (-inf + nanj), not an error.
        let (re, im) = parts(call(
            "log",
            &[
                value::encode_int(0).expect("int"),
                value::encode_int(2).expect("int"),
            ],
        ));
        assert!(re.is_infinite() && re < 0.0 && im.is_nan());
        assert_eq!(
            call("isclose", &[comp(1.0, 1.0), comp(1.0, 1.0)]),
            value::TRUE
        );
        assert_eq!(
            call("isclose", &[comp(NAN, 0.0), comp(NAN, 0.0)]),
            value::FALSE
        );
        assert_eq!(
            call(
                "isclose",
                &[
                    comp(1.0, 0.0),
                    comp(1.0, 1e-10),
                    value::float_new(0.0),
                    value::float_new(1e-9)
                ]
            ),
            value::TRUE
        );
        let keywords = [("abs_tol".to_string(), value::float_new(1e-9))];
        assert_eq!(
            call_kw("isclose", &[comp(1.0, 0.0), comp(1.0, 1e-10)], &keywords),
            value::TRUE
        );
    }

    #[test]
    fn constants() {
        exc::init();
        assert!(close(real(constant("pi").expect("pi")), PI));
        assert!(close(real(constant("e").expect("e")), E));
        assert!(close(real(constant("tau").expect("tau")), TAU));
        assert!(real(constant("inf").expect("inf")).is_infinite());
        assert!(real(constant("nan").expect("nan")).is_nan());
        let (re, im) = parts(constant("infj").expect("infj"));
        assert_eq!(re, 0.0);
        assert!(im.is_infinite());
        let (re, im) = parts(constant("nanj").expect("nanj"));
        assert_eq!(re, 0.0);
        assert!(im.is_nan());
        assert_eq!(constant("nope"), None);
    }
}
