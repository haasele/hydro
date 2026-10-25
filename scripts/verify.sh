#!/usr/bin/env bash
# Full phase gate. Run from anywhere; requires rustc/cargo (rustup).
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."

echo "== fmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== build =="
cargo build -p pyx-cli -p pyx-runtime

echo "== unit/integration tests =="
cargo test --workspace

echo "== e2e =="
PYX="target/debug/pyx"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
expect() {
    local file="$1" expected="$2"
    local out
    out="$("$PYX" run "$file")"
    if [[ "$out" != "$expected" ]]; then
        echo "FAIL: $file" >&2
        diff <(printf '%s\n' "$expected") <(printf '%s\n' "$out") >&2 || true
        exit 1
    fi
    echo "ok: $file"
}

expect examples/hello.py "Hello, PyX!"
expect examples/fib.py $'0\n1\n1\n2\n3\n5\n8\n13\n21\n34'
expect examples/control.py $'14\n3\nlarge\n3\n2\n1'
expect examples/strings.py $'PyX\n3\nn=42\n8\n5.0\nTrue\n0.5\n9'
expect examples/reraise.py $'caught original\nabcabc xyxy z'
expect examples/key_error.py "key 'k'"
expect examples/return_after_try.py "5"
expect examples/collections.py $'[10, 20, 30]\n10 30 3\n[10, 99, 30]\nTrue [10, 99, 30, 40]\n139\n3\n{\'a\': 10, \'b\': 2, \'c\': 3}\n3 2\na\nb\nc\nmissing \'missing\'\nabc'
expect examples/finally.py "$(cat examples/expected/finally.txt)"
expect examples/finally_reraise.py $'finally-reraise reraise-finally\nno-active No active exception to reraise'

"$PYX" run examples/slices.py > "$tmp/slices.out"
diff -u examples/expected/slices.txt "$tmp/slices.out"
echo "ok: slices"

"$PYX" run examples/fstrings.py > "$tmp/fstrings.out"
diff -u examples/expected/fstrings.txt "$tmp/fstrings.out"
echo "ok: fstrings"

"$PYX" run examples/methods.py > "$tmp/methods.out"
diff -u examples/expected/methods.txt "$tmp/methods.out"
echo "ok: methods"

"$PYX" run examples/tuples.py > "$tmp/tuples.out"
diff -u examples/expected/tuples.txt "$tmp/tuples.out"
echo "ok: tuples"

"$PYX" run examples/sets.py > "$tmp/sets.out"
diff -u examples/expected/sets.txt "$tmp/sets.out"
echo "ok: sets"

"$PYX" run examples/builtins.py > "$tmp/builtins.out"
diff -u examples/expected/builtins.txt "$tmp/builtins.out"
echo "ok: builtins"

"$PYX" run examples/comprehensions.py > "$tmp/comprehensions.out"
diff -u examples/expected/comprehensions.txt "$tmp/comprehensions.out"
echo "ok: comprehensions"

"$PYX" run examples/functions.py > "$tmp/functions.out"
diff -u examples/expected/functions.txt "$tmp/functions.out"
echo "ok: functions"

"$PYX" run examples/modules.py > "$tmp/modules.out"
diff -u examples/expected/modules.txt "$tmp/modules.out"
echo "ok: modules"

"$PYX" run examples/json_os.py > "$tmp/json_os.out"
diff -u examples/expected/json_os.txt "$tmp/json_os.out"
echo "ok: json_os"

"$PYX" run examples/threads.py > "$tmp/threads.out"
diff -u examples/expected/threads.txt "$tmp/threads.out"
echo "ok: threads"

PYX_GC_THRESHOLD=1 "$PYX" run examples/threads_gc.py > "$tmp/threads_gc.out"
diff -u examples/expected/threads_gc.txt "$tmp/threads_gc.out"
echo "ok: threads_gc (STW)"

for name in threading event_queue rlock pickle multiprocessing spawn pool pool_spawn files csv_json bytes binary_files stdlib_utils stdlib_modules socket_echo pathlib_example formatting classes generators_async async_generators closures bigint; do
    "$PYX" run "examples/$name.py" > "$tmp/$name.out"
    diff -u "examples/expected/$name.txt" "$tmp/$name.out"
    echo "ok: $name"
done

echo "== negative compile =="
check_fails() {
    local file="$1" needle="$2"
    if "$PYX" check "$file" >"$tmp/check.out" 2>"$tmp/check.err"; then
        echo "FAIL: $file must fail to compile" >&2
        exit 1
    fi
    grep -qi "$needle" "$tmp/check.err" || {
        echo "FAIL: $file missing diagnostic '$needle'" >&2
        cat "$tmp/check.err" >&2
        exit 1
    }
    echo "ok: $file"
}
check_fails examples/errors/format_spec.py "dynamic"
check_fails examples/errors/dict_slice.py "unhashable"
check_fails examples/errors/slice_assign.py "slice assignment"
check_fails examples/errors/unknown_method.py "has no method"
check_fails examples/errors/method_arity.py "argument"
# A method value is a data-attribute read now: it compiles and raises at runtime.
if "$PYX" run examples/errors/attribute_value.py >"$tmp/check.out" 2>"$tmp/check.err"; then
    echo "FAIL: examples/errors/attribute_value.py must fail at runtime" >&2
    exit 1
fi
grep -qi "AttributeError" "$tmp/check.err" || {
    echo "FAIL: examples/errors/attribute_value.py missing AttributeError" >&2
    cat "$tmp/check.err" >&2
    exit 1
}
echo "ok: examples/errors/attribute_value.py"
check_fails examples/errors/method_arg_type.py "wrong type"
check_fails examples/errors/tuple_star.py "starred unpacking"
check_fails examples/errors/tuple_assign.py "item assignment"
check_fails examples/errors/tuple_unhashable_key.py "unhashable"
check_fails examples/errors/set_unhashable.py "unhashable"
check_fails examples/errors/set_subscript.py "not subscriptable"
check_fails examples/errors/in_str_type.py "string"
check_fails examples/errors/in_noniterable.py "not iterable"
check_fails examples/errors/set_index_assign.py "does not support item assignment"
check_fails examples/errors/list_noniterable.py "iterable"
check_fails examples/errors/dict_noniterable.py "iterable"
check_fails examples/errors/range_arg_type.py "range"
check_fails examples/errors/sum_arg_type.py "number"
check_fails examples/errors/comp_noniterable.py "not iterable"
check_fails examples/errors/comp_unhashable.py "unhashable"
check_fails examples/errors/lambda_closure.py "closures"
check_fails examples/errors/map_noncallable.py "callable"
check_fails examples/errors/filter_noncallable.py "callable"
check_fails examples/errors/indirect_noncallable.py "not callable"
check_fails examples/errors/lambda_default.py "default"
check_fails examples/errors/lambda_star.py "varargs"
check_fails examples/errors/import_unsupported.py "not supported"
check_fails examples/errors/module_member_unknown.py "no attribute"
# Function-local imports bind native modules; this fixture exercises it.
"$PYX" run examples/errors/import_in_function.py >"$tmp/check.out" 2>"$tmp/check.err" || {
    echo "FAIL: examples/errors/import_in_function.py must run" >&2
    cat "$tmp/check.err" >&2
    exit 1
}
grep -q "3.14" "$tmp/check.out" || {
    echo "FAIL: examples/errors/import_in_function.py unexpected output" >&2
    cat "$tmp/check.out" >&2
    exit 1
}
echo "ok: examples/errors/import_in_function.py"
check_fails examples/errors/from_member_unknown.py "no attribute"
# Standard-library modules are first-class values now (getattr/argv cases).
"$PYX" check examples/errors/module_value.py >"$tmp/check.out" 2>"$tmp/check.err" || {
    echo "FAIL: examples/errors/module_value.py must compile" >&2
    cat "$tmp/check.err" >&2
    exit 1
}
echo "ok: examples/errors/module_value.py"
check_fails examples/errors/module_call_noncallable.py "not callable"
check_fails examples/errors/json_member_unknown.py "no attribute"
check_fails examples/errors/os_member_unknown.py "no attribute"

echo "== negative runtime =="
fail_out="$tmp/fail.out"
fail_err="$tmp/fail.err"
if "$PYX" run examples/errors/index_error.py >"$fail_out" 2>"$fail_err"; then
    echo "FAIL: index_error.py must exit non-zero" >&2
    exit 1
fi
grep -q "IndexError: list index out of range" "$fail_err" || {
    echo "FAIL: missing IndexError traceback" >&2
    cat "$fail_err" >&2
    exit 1
}
echo "ok: index_error.py"

echo "== gc =="
gc_out="$(PYX_GC_THRESHOLD=16 "$PYX" run examples/gc_stress.py)"
if [[ "$gc_out" != "1600" ]]; then
    echo "FAIL: gc threshold run (got '$gc_out')" >&2
    exit 1
fi
gc_stress_out="$(PYX_GC_STRESS=1 "$PYX" run examples/gc_stress.py)"
if [[ "$gc_stress_out" != "1600" ]]; then
    echo "FAIL: gc stress run (got '$gc_stress_out')" >&2
    exit 1
fi
collections_base="$("$PYX" run examples/collections.py)"
collections_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/collections.py)"
if [[ "$collections_gc" != "$collections_base" ]]; then
    echo "FAIL: collections output differs under GC pressure" >&2
    diff <(printf '%s\n' "$collections_base") <(printf '%s\n' "$collections_gc") >&2 || true
    exit 1
fi
echo "ok: gc threshold/stress/container roots"
finally_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/finally.py)"
finally_expected="$(cat examples/expected/finally.txt)"
if [[ "$finally_gc" != "$finally_expected" ]]; then
    echo "FAIL: finally output differs under GC pressure" >&2
    diff <(printf '%s\n' "$finally_expected") <(printf '%s\n' "$finally_gc") >&2 || true
    exit 1
fi
echo "ok: gc finally"
slices_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/slices.py)"
if [[ "$slices_gc" != "$(cat examples/expected/slices.txt)" ]]; then
    echo "FAIL: slices output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc slices"
methods_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/methods.py)"
if [[ "$methods_gc" != "$(cat examples/expected/methods.txt)" ]]; then
    echo "FAIL: methods output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc methods"
tuples_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/tuples.py)"
if [[ "$tuples_gc" != "$(cat examples/expected/tuples.txt)" ]]; then
    echo "FAIL: tuples output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc tuples"
sets_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/sets.py)"
if [[ "$sets_gc" != "$(cat examples/expected/sets.txt)" ]]; then
    echo "FAIL: sets output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc sets"
builtins_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/builtins.py)"
if [[ "$builtins_gc" != "$(cat examples/expected/builtins.txt)" ]]; then
    echo "FAIL: builtins output differs under GC pressure" >&2
    exit 1
fi
if "$PYX" run examples/errors/range_step_zero.py >"$tmp/rz.out" 2>"$tmp/rz.err"; then
    echo "FAIL: range_step_zero.py must exit non-zero" >&2
    exit 1
fi
grep -q "range() arg 3 must not be zero" "$tmp/rz.err" || {
    echo "FAIL: missing range step ValueError" >&2
    exit 1
}
echo "ok: gc builtins + range errors"
comprehensions_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/comprehensions.py)"
if [[ "$comprehensions_gc" != "$(cat examples/expected/comprehensions.txt)" ]]; then
    echo "FAIL: comprehensions output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc comprehensions"
functions_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/functions.py)"
if [[ "$functions_gc" != "$(cat examples/expected/functions.txt)" ]]; then
    echo "FAIL: functions output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc functions"
modules_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/modules.py)"
if [[ "$modules_gc" != "$(cat examples/expected/modules.txt)" ]]; then
    echo "FAIL: modules output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc modules"
json_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/json_os.py)"
if [[ "$json_gc" != "$(cat examples/expected/json_os.txt)" ]]; then
    echo "FAIL: json_os output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc json_os"
threads_gc="$(PYX_GC_THRESHOLD=8 "$PYX" run examples/threads.py)"
if [[ "$threads_gc" != "$(cat examples/expected/threads.txt)" ]]; then
    echo "FAIL: threads output differs under GC pressure" >&2
    exit 1
fi
echo "ok: gc threads"
PYX_GC_THRESHOLD=1 "$PYX" run examples/threads_gc.py > "$tmp/threads_gc2.out" 2>"$tmp/threads_gc2.err"
diff -u examples/expected/threads_gc.txt "$tmp/threads_gc2.out"
if [[ -s "$tmp/threads_gc2.err" ]]; then
    echo "FAIL: threads_gc stderr not empty" >&2
    cat "$tmp/threads_gc2.err" >&2
    exit 1
fi
echo "ok: stw threads_gc"

echo "== aot =="
"$PYX" build examples/hello.py -o "$tmp/hello"
run_out="$("$tmp/hello")"
if [[ "$run_out" != "Hello, PyX!" ]]; then
    echo "FAIL: aot hello" >&2
    exit 1
fi
echo "ok: aot hello"

"$PYX" build examples/fib.py -o "$tmp/fib"
aot_fib="$("$tmp/fib")"
jit_fib="$("$PYX" run examples/fib.py)"
if [[ "$aot_fib" != "$jit_fib" ]]; then
    echo "FAIL: aot fib differs from JIT" >&2
    diff <(printf '%s\n' "$jit_fib") <(printf '%s\n' "$aot_fib") >&2 || true
    exit 1
fi
echo "ok: aot fib"

"$PYX" build examples/collections.py -o "$tmp/collections"
aot_collections="$("$tmp/collections")"
jit_collections="$("$PYX" run examples/collections.py)"
if [[ "$aot_collections" != "$jit_collections" ]]; then
    echo "FAIL: aot collections differs from JIT" >&2
    diff <(printf '%s\n' "$jit_collections") <(printf '%s\n' "$aot_collections") >&2 || true
    exit 1
fi
echo "ok: aot collections"

"$PYX" build examples/gc_stress.py -o "$tmp/gc_stress"
if [[ "$(PYX_GC_THRESHOLD=16 "$tmp/gc_stress")" != "1600" ]]; then
    echo "FAIL: aot gc_stress" >&2
    exit 1
fi
echo "ok: aot gc_stress"

"$PYX" build examples/finally.py -o "$tmp/finally"
aot_finally="$(PYX_GC_THRESHOLD=8 "$tmp/finally")"
if [[ "$aot_finally" != "$(cat examples/expected/finally.txt)" ]]; then
    echo "FAIL: aot finally" >&2
    exit 1
fi
echo "ok: aot finally"

"$PYX" build examples/slices.py -o "$tmp/slices"
"$tmp/slices" > "$tmp/slices.aot.out"
diff -u examples/expected/slices.txt "$tmp/slices.aot.out"
echo "ok: aot slices"

"$PYX" build examples/fstrings.py -o "$tmp/fstrings"
"$tmp/fstrings" > "$tmp/fstrings.aot.out"
diff -u examples/expected/fstrings.txt "$tmp/fstrings.aot.out"
echo "ok: aot fstrings"

"$PYX" build examples/methods.py -o "$tmp/methods"
"$tmp/methods" > "$tmp/methods.aot.out"
diff -u examples/expected/methods.txt "$tmp/methods.aot.out"
echo "ok: aot methods"

"$PYX" build examples/tuples.py -o "$tmp/tuples"
"$tmp/tuples" > "$tmp/tuples.aot.out"
diff -u examples/expected/tuples.txt "$tmp/tuples.aot.out"
echo "ok: aot tuples"

"$PYX" build examples/sets.py -o "$tmp/sets"
"$tmp/sets" > "$tmp/sets.aot.out"
diff -u examples/expected/sets.txt "$tmp/sets.aot.out"
echo "ok: aot sets"

"$PYX" build examples/builtins.py -o "$tmp/builtins"
"$tmp/builtins" > "$tmp/builtins.aot.out"
diff -u examples/expected/builtins.txt "$tmp/builtins.aot.out"
echo "ok: aot builtins"

"$PYX" build examples/comprehensions.py -o "$tmp/comprehensions"
"$tmp/comprehensions" > "$tmp/comprehensions.aot.out"
diff -u examples/expected/comprehensions.txt "$tmp/comprehensions.aot.out"
echo "ok: aot comprehensions"

"$PYX" build examples/functions.py -o "$tmp/functions"
"$tmp/functions" > "$tmp/functions.aot.out"
diff -u examples/expected/functions.txt "$tmp/functions.aot.out"
echo "ok: aot functions"

"$PYX" build examples/modules.py -o "$tmp/modules"
"$tmp/modules" > "$tmp/modules.aot.out"
diff -u examples/expected/modules.txt "$tmp/modules.aot.out"
echo "ok: aot modules"

"$PYX" build examples/json_os.py -o "$tmp/json_os"
"$tmp/json_os" > "$tmp/json_os.aot.out"
diff -u examples/expected/json_os.txt "$tmp/json_os.aot.out"
echo "ok: aot json_os"

"$PYX" build examples/threads.py -o "$tmp/threads"
"$tmp/threads" > "$tmp/threads.aot.out"
diff -u examples/expected/threads.txt "$tmp/threads.aot.out"
echo "ok: aot threads"

"$PYX" build examples/threads_gc.py -o "$tmp/threads_gc"
PYX_GC_THRESHOLD=1 "$tmp/threads_gc" > "$tmp/threads_gc.aot.out"
diff -u examples/expected/threads_gc.txt "$tmp/threads_gc.aot.out"
echo "ok: aot threads_gc (STW)"

for name in threading event_queue rlock pickle multiprocessing spawn pool pool_spawn files csv_json bytes binary_files stdlib_utils stdlib_modules socket_echo pathlib_example formatting classes generators_async async_generators closures bigint; do
    "$PYX" build "examples/$name.py" -o "$tmp/$name"
    "$tmp/$name" > "$tmp/$name.aot.out"
    diff -u "examples/expected/$name.txt" "$tmp/$name.aot.out"
    echo "ok: aot $name"
done

python3 scripts/cpython_compat.py --check-manifest

echo "ALL GREEN"
