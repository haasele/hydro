//! `_opcode` / `_opcode_metadata`: the CPython 3.14 instruction tables.
//!
//! Hydro does not execute CPython bytecode, but CPython's pure-Python
//! `opcode.py` and `dis.py` build their public tables from these two modules,
//! and the CPython 3.14 test suites (`test__opcode`, `test_dis`,
//! `test_peepholer`) assert against them. The tables below are the exact
//! 3.14.9 reference-interpreter data: opcode names/numbers (including the
//! specialized and instrumented ranges), the argument/constant/name/jump/
//! free/local/exception classification, opcode validity, and the stack effect
//! of every instruction. `stack_effect` therefore answers with the same values
//! the reference compiler reports, including the oparg-dependent families
//! (`BUILD_*`, `CALL*`, `UNPACK_*`, `RAISE_VARARGS`, `LOAD_GLOBAL`, ...).
//!
//! Truthful limits of this runtime remain explicit:
//!
//! * `get_executor()` raises `NotImplementedError`: Hydro executes its own
//!   HIR, not CPython bytecode, so no executor object exists;
//! * `get_specialization_stats()` returns `None`, the documented answer when
//!   the interpreter exposes no specialization statistics;
//! * the intrinsic/special-method/number-operation tables are the reference
//!   tables, so `opcode.py` builds `dis`'s metadata exactly.

use crate::container;
use crate::exc::{self, ATTRIBUTE_ERROR, NOT_IMPLEMENTED_ERROR, TYPE_ERROR, VALUE_ERROR};
use crate::heap;
use crate::ops;
use crate::value::{self, NONE};

pub(crate) const ENABLE_SPECIALIZATION: i64 = 1;
pub(crate) const ENABLE_SPECIALIZATION_FT: i64 = 1;

/// `opcode.HAVE_ARGUMENT`.
pub(crate) const HAVE_ARGUMENT: i64 = 43;
/// `opcode.MIN_INSTRUMENTED_OPCODE`.
pub(crate) const MIN_INSTRUMENTED_OPCODE: i64 = 234;

/// Predicate bit assignments of [`PREDICATES`].
const HAS_ARG: u8 = 1 << 0;
const HAS_CONST: u8 = 1 << 1;
const HAS_NAME: u8 = 1 << 2;
const HAS_JUMP: u8 = 1 << 3;
const HAS_FREE: u8 = 1 << 4;
const HAS_LOCAL: u8 = 1 << 5;
const HAS_EXC: u8 = 1 << 6;
const IS_VALID: u8 = 1 << 7;

/// `None`-equivalent entry of [`STACK_EFFECTS`].
const INVALID_EFFECT: i8 = i8::MIN;

#[cfg(test)]
pub(crate) const OPMAP: &[(&str, u16)] = &[
    ("CACHE", 0), ("BINARY_SLICE", 1), ("BUILD_TEMPLATE", 2), ("CALL_FUNCTION_EX", 4),
    ("CHECK_EG_MATCH", 5), ("CHECK_EXC_MATCH", 6), ("CLEANUP_THROW", 7), ("DELETE_SUBSCR", 8),
    ("END_FOR", 9), ("END_SEND", 10), ("EXIT_INIT_CHECK", 11), ("FORMAT_SIMPLE", 12),
    ("FORMAT_WITH_SPEC", 13), ("GET_AITER", 14), ("GET_ANEXT", 15), ("GET_ITER", 16),
    ("RESERVED", 17), ("GET_LEN", 18), ("GET_YIELD_FROM_ITER", 19), ("INTERPRETER_EXIT", 20),
    ("LOAD_BUILD_CLASS", 21), ("LOAD_LOCALS", 22), ("MAKE_FUNCTION", 23), ("MATCH_KEYS", 24),
    ("MATCH_MAPPING", 25), ("MATCH_SEQUENCE", 26), ("NOP", 27), ("NOT_TAKEN", 28),
    ("POP_EXCEPT", 29), ("POP_ITER", 30), ("POP_TOP", 31), ("PUSH_EXC_INFO", 32),
    ("PUSH_NULL", 33), ("RETURN_GENERATOR", 34), ("RETURN_VALUE", 35), ("SETUP_ANNOTATIONS", 36),
    ("STORE_SLICE", 37), ("STORE_SUBSCR", 38), ("TO_BOOL", 39), ("UNARY_INVERT", 40),
    ("UNARY_NEGATIVE", 41), ("UNARY_NOT", 42), ("WITH_EXCEPT_START", 43), ("BINARY_OP", 44),
    ("BUILD_INTERPOLATION", 45), ("BUILD_LIST", 46), ("BUILD_MAP", 47), ("BUILD_SET", 48),
    ("BUILD_SLICE", 49), ("BUILD_STRING", 50), ("BUILD_TUPLE", 51), ("CALL", 52),
    ("CALL_INTRINSIC_1", 53), ("CALL_INTRINSIC_2", 54), ("CALL_KW", 55), ("COMPARE_OP", 56),
    ("CONTAINS_OP", 57), ("CONVERT_VALUE", 58), ("COPY", 59), ("COPY_FREE_VARS", 60),
    ("DELETE_ATTR", 61), ("DELETE_DEREF", 62), ("DELETE_FAST", 63), ("DELETE_GLOBAL", 64),
    ("DELETE_NAME", 65), ("DICT_MERGE", 66), ("DICT_UPDATE", 67), ("END_ASYNC_FOR", 68),
    ("EXTENDED_ARG", 69), ("FOR_ITER", 70), ("GET_AWAITABLE", 71), ("IMPORT_FROM", 72),
    ("IMPORT_NAME", 73), ("IS_OP", 74), ("JUMP_BACKWARD", 75), ("JUMP_BACKWARD_NO_INTERRUPT", 76),
    ("JUMP_FORWARD", 77), ("LIST_APPEND", 78), ("LIST_EXTEND", 79), ("LOAD_ATTR", 80),
    ("LOAD_COMMON_CONSTANT", 81), ("LOAD_CONST", 82), ("LOAD_DEREF", 83), ("LOAD_FAST", 84),
    ("LOAD_FAST_AND_CLEAR", 85), ("LOAD_FAST_BORROW", 86), ("LOAD_FAST_BORROW_LOAD_FAST_BORROW", 87), ("LOAD_FAST_CHECK", 88),
    ("LOAD_FAST_LOAD_FAST", 89), ("LOAD_FROM_DICT_OR_DEREF", 90), ("LOAD_FROM_DICT_OR_GLOBALS", 91), ("LOAD_GLOBAL", 92),
    ("LOAD_NAME", 93), ("LOAD_SMALL_INT", 94), ("LOAD_SPECIAL", 95), ("LOAD_SUPER_ATTR", 96),
    ("MAKE_CELL", 97), ("MAP_ADD", 98), ("MATCH_CLASS", 99), ("POP_JUMP_IF_FALSE", 100),
    ("POP_JUMP_IF_NONE", 101), ("POP_JUMP_IF_NOT_NONE", 102), ("POP_JUMP_IF_TRUE", 103), ("RAISE_VARARGS", 104),
    ("RERAISE", 105), ("SEND", 106), ("SET_ADD", 107), ("SET_FUNCTION_ATTRIBUTE", 108),
    ("SET_UPDATE", 109), ("STORE_ATTR", 110), ("STORE_DEREF", 111), ("STORE_FAST", 112),
    ("STORE_FAST_LOAD_FAST", 113), ("STORE_FAST_STORE_FAST", 114), ("STORE_GLOBAL", 115), ("STORE_NAME", 116),
    ("SWAP", 117), ("UNPACK_EX", 118), ("UNPACK_SEQUENCE", 119), ("YIELD_VALUE", 120),
    ("RESUME", 128), ("INSTRUMENTED_END_FOR", 234), ("INSTRUMENTED_POP_ITER", 235), ("INSTRUMENTED_END_SEND", 236),
    ("INSTRUMENTED_FOR_ITER", 237), ("INSTRUMENTED_INSTRUCTION", 238), ("INSTRUMENTED_JUMP_FORWARD", 239), ("INSTRUMENTED_NOT_TAKEN", 240),
    ("INSTRUMENTED_POP_JUMP_IF_TRUE", 241), ("INSTRUMENTED_POP_JUMP_IF_FALSE", 242), ("INSTRUMENTED_POP_JUMP_IF_NONE", 243), ("INSTRUMENTED_POP_JUMP_IF_NOT_NONE", 244),
    ("INSTRUMENTED_RESUME", 245), ("INSTRUMENTED_RETURN_VALUE", 246), ("INSTRUMENTED_YIELD_VALUE", 247), ("INSTRUMENTED_END_ASYNC_FOR", 248),
    ("INSTRUMENTED_LOAD_SUPER_ATTR", 249), ("INSTRUMENTED_CALL", 250), ("INSTRUMENTED_CALL_KW", 251), ("INSTRUMENTED_CALL_FUNCTION_EX", 252),
    ("INSTRUMENTED_JUMP_BACKWARD", 253), ("INSTRUMENTED_LINE", 254), ("ENTER_EXECUTOR", 255), ("ANNOTATIONS_PLACEHOLDER", 256),
    ("JUMP", 257), ("JUMP_IF_FALSE", 258), ("JUMP_IF_TRUE", 259), ("JUMP_NO_INTERRUPT", 260),
    ("LOAD_CLOSURE", 261), ("POP_BLOCK", 262), ("SETUP_CLEANUP", 263), ("SETUP_FINALLY", 264),
    ("SETUP_WITH", 265), ("STORE_FAST_MAYBE_NULL", 266),
];

/// `_opcode`'s per-opcode answers, one byte per opcode 0..=266.
///
/// Bit layout: `has_arg`, `has_const`, `has_name`, `has_jump`, `has_free`,
/// `has_local`, `has_exc`, `is_valid` (low to high).
const PREDICATES: [u8; 267] = [
    128, 128, 128, 160, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
    128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
    128, 128, 128, 128, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129,
    129, 133, 145, 161, 133, 133, 129, 129, 137, 129, 137, 129, 133, 133, 129, 137, 137, 137, 129, 129,
    133, 129, 131, 161, 161, 161, 161, 161, 161, 161, 145, 133, 133, 133, 129, 129, 133, 145, 129, 129,
    137, 137, 137, 137, 129, 129, 137, 129, 129, 129, 133, 145, 161, 161, 161, 133, 133, 129, 129, 129,
    129, 0, 0, 0, 0, 0, 0, 0, 129, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128, 128,
    128, 128, 128, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 128, 129, 129, 129, 129, 129,
    129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 129, 137, 137, 137, 137, 137, 129, 129, 133,
    129, 129, 129, 129, 129, 129, 129, 129, 129, 133, 131, 131, 129, 129, 133, 133, 128, 129, 128, 128,
    133, 128, 128, 128, 128, 128, 128, 128, 128, 129, 129, 129, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 128, 128, 137, 128, 129,
    128, 129, 129, 129, 129, 129, 128, 129, 137, 133, 129, 129, 128, 129, 128, 129, 128, 137, 137, 137,
    137, 161, 128, 193, 193, 193, 161,
];

/// Net stack effect of every opcode at `oparg=0` (and at every oparg for the
/// opcodes whose effect does not depend on it). `INVALID_EFFECT` marks
/// opcodes `stack_effect` rejects (specialized and undefined instructions).
const STACK_EFFECTS: [i8; 267] = [
    0, -2, -1, -128, -3, 0, 0, -1, -2, -1, -1, -1, 0, -1, 0, 1, 0, 0, 1, 0,
    -1, 1, 1, 0, 1, 1, 1, 0, 0, -1, -1, -1, 1, 1, 1, 0, 0, -4, -3, 0,
    0, 0, 0, 1, -1, -1, 1, 1, 1, 1, 1, 1, -1, 0, -1, -2, -1, -1, 0, 1,
    0, -1, 0, 0, 0, 0, -1, -1, -2, 0, 1, 0, 1, -1, -1, 0, 0, 0, -1, -1,
    0, 1, 1, 1, 1, 1, 1, 2, 1, 2, 0, 0, 1, 1, 1, 1, -2, 0, -2, -2,
    -1, -1, -1, -1, 0, -1, 0, -1, -1, -1, -2, -1, -1, 0, -2, -1, -1, 0, 0, -1,
    0, -128, -128, -128, -128, -128, -128, -128, 0, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128,
    -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128,
    -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128,
    -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128,
    -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128,
    -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -128, -1, -1, -1, 1, 0, 0,
    0, -1, -1, -1, -1, 0, 0, 0, -2, -2, -1, -2, -3, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 0, 2, 1, 1, -1,
];

/// Bits set for opcodes that are specialized (or deoptimized) forms.
const SPECIALIZED_MASK: [u8; 267] = [
    0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const INTRINSIC_1_DESCS: &[&str] = &[
    "INTRINSIC_1_INVALID",
    "INTRINSIC_PRINT",
    "INTRINSIC_IMPORT_STAR",
    "INTRINSIC_STOPITERATION_ERROR",
    "INTRINSIC_ASYNC_GEN_WRAP",
    "INTRINSIC_UNARY_POSITIVE",
    "INTRINSIC_LIST_TO_TUPLE",
    "INTRINSIC_TYPEVAR",
    "INTRINSIC_PARAMSPEC",
    "INTRINSIC_TYPEVARTUPLE",
    "INTRINSIC_SUBSCRIPT_GENERIC",
    "INTRINSIC_TYPEALIAS",
];

const INTRINSIC_2_DESCS: &[&str] = &[
    "INTRINSIC_2_INVALID",
    "INTRINSIC_PREP_RERAISE_STAR",
    "INTRINSIC_TYPEVAR_WITH_BOUND",
    "INTRINSIC_TYPEVAR_WITH_CONSTRAINTS",
    "INTRINSIC_SET_FUNCTION_TYPE_PARAMS",
    "INTRINSIC_SET_TYPEPARAM_DEFAULT",
];

const SPECIAL_METHOD_NAMES: &[&str] = &["__enter__", "__exit__", "__aenter__", "__aexit__"];

const NB_OPS: &[(&str, &str)] = &[
    ("NB_ADD", "+"),
    ("NB_AND", "&"),
    ("NB_FLOOR_DIVIDE", "//"),
    ("NB_LSHIFT", "<<"),
    ("NB_MATRIX_MULTIPLY", "@"),
    ("NB_MULTIPLY", "*"),
    ("NB_REMAINDER", "%"),
    ("NB_OR", "|"),
    ("NB_POWER", "**"),
    ("NB_RSHIFT", ">>"),
    ("NB_SUBTRACT", "-"),
    ("NB_TRUE_DIVIDE", "/"),
    ("NB_XOR", "^"),
    ("NB_INPLACE_ADD", "+="),
    ("NB_INPLACE_AND", "&="),
    ("NB_INPLACE_FLOOR_DIVIDE", "//="),
    ("NB_INPLACE_LSHIFT", "<<="),
    ("NB_INPLACE_MATRIX_MULTIPLY", "@="),
    ("NB_INPLACE_MULTIPLY", "*="),
    ("NB_INPLACE_REMAINDER", "%="),
    ("NB_INPLACE_OR", "|="),
    ("NB_INPLACE_POWER", "**="),
    ("NB_INPLACE_RSHIFT", ">>="),
    ("NB_INPLACE_SUBTRACT", "-="),
    ("NB_INPLACE_TRUE_DIVIDE", "/="),
    ("NB_INPLACE_XOR", "^="),
    ("NB_SUBSCR", "[]"),
];

pub(crate) fn constant(name: &str) -> i64 {
    match name {
        "ENABLE_SPECIALIZATION" => ops::int_result(ENABLE_SPECIALIZATION),
        "ENABLE_SPECIALIZATION_FT" => ops::int_result(ENABLE_SPECIALIZATION_FT),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_opcode' has no attribute '{name}'"),
        ),
    }
}

fn list_of_strings(items: &[&str]) -> i64 {
    let list = value::list_new(Vec::with_capacity(items.len()));
    let roots = heap::root_values(&[list]);
    for &item in items {
        let text = value::str_alloc(item.to_string());
        container::list_append(list, text);
    }
    heap::release_roots(roots);
    list
}

fn nb_ops_list() -> i64 {
    let mut values = Vec::with_capacity(NB_OPS.len());
    for &(name, symbol) in NB_OPS {
        let first = value::str_alloc(name.to_string());
        let second = value::str_alloc(symbol.to_string());
        values.push((first, second));
    }
    let roots: Vec<i64> = values.iter().flat_map(|&(a, b)| [a, b]).collect();
    let root_start = heap::root_values(&roots);
    let tuples: Vec<i64> = values
        .iter()
        .map(|&(first, second)| value::tuple_new(vec![first, second]))
        .collect();
    let root_start2 = heap::root_values(&tuples);
    let list = value::list_new(tuples);
    heap::release_roots(root_start2);
    heap::release_roots(root_start);
    list
}

/// Integer argument of a predicate/`is_valid` call.
fn opcode_argument(function: &str, argument: i64) -> Result<i64, i64> {
    match value::as_integer(argument) {
        Some(number) => Ok(number),
        None => Err(exc::raise_with_message(
            TYPE_ERROR,
            &format!(
                "{function}() argument must be int, not {}",
                value::type_name(argument)
            ),
        )),
    }
}

/// `_opcode.<predicate>(opcode)`: the documented classification.
fn predicate(function: &str, argument: i64) -> i64 {
    let code = match opcode_argument(function, argument) {
        Ok(code) => code,
        Err(error) => return error,
    };
    let bit = match function {
        "has_arg" => HAS_ARG,
        "has_const" => HAS_CONST,
        "has_name" => HAS_NAME,
        "has_jump" => HAS_JUMP,
        "has_free" => HAS_FREE,
        "has_local" => HAS_LOCAL,
        "has_exc" => HAS_EXC,
        "is_valid" => IS_VALID,
        _ => IS_VALID,
    };
    let valid = usize::try_from(code)
        .ok()
        .and_then(|index| PREDICATES.get(index).copied())
        .is_some_and(|flags| flags & bit != 0);
    value::bool_value(valid)
}

/// `_opcode.stack_effect(opcode, oparg=None, *, jump=None)`.
///
/// The oparg-dependent instruction families are evaluated with CPython's own
/// formulas; every other instruction uses the reference table, which was
/// captured at `oparg=0` (their effect does not depend on the argument).
fn stack_effect(args: &[i64]) -> i64 {
    let Some(&argument) = args.first() else {
        return exc::raise_with_message(TYPE_ERROR, "stack_effect() needs an opcode");
    };
    let opcode = match opcode_argument("stack_effect", argument) {
        Ok(code) => code,
        Err(error) => return error,
    };
    let oparg = match args.get(1).copied() {
        None | Some(NONE) => 0,
        Some(argument) => match value::as_integer(argument) {
            Some(number) => number,
            None => {
                return exc::raise_with_message(
                    TYPE_ERROR,
                    &format!(
                        "stack_effect() argument 2 must be int, not {}",
                        value::type_name(argument)
                    ),
                )
            }
        },
    };
    match args.get(2).copied() {
        None | Some(NONE) => {}
        Some(argument) if argument == value::TRUE || argument == value::FALSE => {}
        Some(_) => {
            return exc::raise_with_message(
                VALUE_ERROR,
                "stack_effect: jump must be False, True or None",
            )
        }
    }
    let Ok(index) = usize::try_from(opcode) else {
        return invalid_effect();
    };
    let Some(&effect) = STACK_EFFECTS.get(index) else {
        return invalid_effect();
    };
    if effect == INVALID_EFFECT || SPECIALIZED_MASK.get(index).copied().unwrap_or(0) != 0 {
        return invalid_effect();
    }
    // Argument-dependent families. `oparg` is bounded to a signed 32-bit value
    // exactly like CPython's `(int)PyLong_AsLong` conversion.
    let Ok(oparg) = i32::try_from(oparg) else {
        return invalid_effect();
    };
    let effect = match index as u32 {
        // BUILD_INTERPOLATION: -1 - (oparg & 1)
        45 => -1 - (oparg & 1) as i64,
        // BUILD_LIST / BUILD_SET / BUILD_SLICE / BUILD_STRING / BUILD_TUPLE
        46 | 48 | 49 | 50 | 51 => 1 - oparg as i64,
        // BUILD_MAP
        47 => 1 - 2 * oparg as i64,
        // CALL / INSTRUMENTED_CALL
        52 | 250 => -1 - oparg as i64,
        // CALL_KW / INSTRUMENTED_CALL_KW
        55 | 251 => -2 - oparg as i64,
        // LOAD_ATTR
        80 => (oparg & 1) as i64,
        // LOAD_GLOBAL
        92 => 1 + (oparg & 1) as i64,
        // LOAD_SUPER_ATTR / INSTRUMENTED_LOAD_SUPER_ATTR
        96 | 249 => -2 + (oparg & 1) as i64,
        // RAISE_VARARGS
        104 => -(oparg as i64),
        // UNPACK_EX
        118 => oparg as i64,
        // UNPACK_SEQUENCE
        119 => oparg as i64 - 1,
        _ => effect as i64,
    };
    ops::int_result(effect)
}

fn invalid_effect() -> i64 {
    exc::raise_with_message(VALUE_ERROR, "invalid opcode or oparg")
}

/// Dispatches a native `_opcode` function.
pub(crate) fn call(function: &str, args: &[i64]) -> i64 {
    match function {
        "has_arg" | "has_const" | "has_name" | "has_jump" | "has_free" | "has_local"
        | "has_exc" | "is_valid" => {
            predicate(function, args.first().copied().unwrap_or(NONE))
        }
        "stack_effect" => stack_effect(args),
        "get_executor" => exc::raise_with_message(
            NOT_IMPLEMENTED_ERROR,
            "get_executor() is not available: Hydro code has no CPython executors",
        ),
        // CPython answers `None` when the interpreter exposes no
        // specialization statistics, which is the documented contract of the
        // 3.14 test.
        "get_specialization_stats" => NONE,
        "get_intrinsic1_descs" => list_of_strings(INTRINSIC_1_DESCS),
        "get_intrinsic2_descs" => list_of_strings(INTRINSIC_2_DESCS),
        "get_special_method_names" => list_of_strings(SPECIAL_METHOD_NAMES),
        "get_nb_ops" => nb_ops_list(),
        _ => exc::raise_with_message(
            ATTRIBUTE_ERROR,
            &format!("module '_opcode' has no attribute '{function}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exc, value};

    fn int(number: i64) -> i64 {
        ops::int_result(number)
    }

    fn code(name: &str) -> i64 {
        OPMAP
            .iter()
            .find(|(entry, _)| *entry == name)
            .map(|(_, code)| *code as i64)
            .unwrap_or_else(|| panic!("{name} is not an opcode"))
    }

    #[test]
    fn specialization_flags_match_the_reference_build() {
        exc::init();
        assert_eq!(constant("ENABLE_SPECIALIZATION"), int(1));
        assert_eq!(constant("ENABLE_SPECIALIZATION_FT"), int(1));
        assert!(!exc::pending());
    }

    #[test]
    fn predicates_report_the_reference_classification() {
        exc::init();
        assert_eq!(call("is_valid", &[int(code("CACHE"))]), value::TRUE);
        assert_eq!(
            call("is_valid", &[int(code("INSTRUMENTED_RETURN_VALUE"))]),
            value::TRUE
        );
        assert_eq!(call("is_valid", &[int(520)]), value::FALSE);
        assert_eq!(call("has_arg", &[int(code("LOAD_CONST"))]), value::TRUE);
        assert_eq!(call("has_const", &[int(code("LOAD_CONST"))]), value::TRUE);
        assert_eq!(call("has_name", &[int(code("IMPORT_NAME"))]), value::TRUE);
        assert_eq!(call("has_jump", &[int(code("JUMP"))]), value::TRUE);
        assert_eq!(call("has_arg", &[int(code("POP_TOP"))]), value::FALSE);
        assert!(!exc::pending());
    }

    #[test]
    fn stack_effect_matches_the_reference_values() {
        exc::init();
        assert_eq!(
            call("stack_effect", &[int(code("POP_TOP"))]),
            int(-1)
        );
        assert_eq!(
            call("stack_effect", &[int(code("BUILD_SLICE")), int(2)]),
            int(-1)
        );
        assert_eq!(
            call("stack_effect", &[int(code("BUILD_SLICE")), int(3)]),
            int(-2)
        );
        assert_eq!(
            call("stack_effect", &[int(code("FOR_ITER")), int(0)]),
            int(1)
        );
        assert_eq!(
            call("stack_effect", &[int(code("JUMP_FORWARD")), int(0)]),
            int(0)
        );
        // Specialized and undefined opcodes are rejected.
        call("stack_effect", &[int(30000)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        call("stack_effect", &[int(129)]);
        assert_eq!(exc::pending_tag(), VALUE_ERROR as i64);
        exc::clear();
        assert!(!exc::pending());
    }

    #[test]
    fn metadata_helpers_report_the_reference_tables() {
        exc::init();
        let intrinsics = call("get_intrinsic1_descs", &[]);
        assert_eq!(
            value::as_list(intrinsics).map(<[i64]>::len),
            Some(INTRINSIC_1_DESCS.len())
        );
        let special = call("get_special_method_names", &[]);
        assert_eq!(
            value::as_list(special).map(<[i64]>::len),
            Some(SPECIAL_METHOD_NAMES.len())
        );
        let ops = call("get_nb_ops", &[]);
        assert_eq!(value::as_list(ops).map(<[i64]>::len), Some(NB_OPS.len()));
        assert_eq!(call("get_specialization_stats", &[]), NONE);
        assert!(!exc::pending());
    }

    #[test]
    fn unknown_names_raise_attribute_error() {
        exc::init();
        call("missing", &[]);
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
        constant("missing");
        assert_eq!(exc::pending_tag(), ATTRIBUTE_ERROR as i64);
        exc::clear();
    }
}
