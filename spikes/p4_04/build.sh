#!/usr/bin/env bash
set -euo pipefail

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$HERE"

CC=${CC:-cc}
PYTHON=${PYTHON:-python3}
PYTHON_CONFIG=${PYTHON_CONFIG:-python3-config}
NM=${NM:-nm}

read -r -a INCLUDES <<< "$("$PYTHON_CONFIG" --includes)"
read -r -a EMBED_LDFLAGS <<< "$("$PYTHON_CONFIG" --embed --ldflags)"

"$CC" -O2 -Wall -Wextra "${INCLUDES[@]}" embed_probe.c \
    -o embed_probe "${EMBED_LDFLAGS[@]}"

"$CC" -O2 -Wall -Wextra -shared -fPIC "${INCLUDES[@]}" abi3_probe.c \
    -o pyx_capi_probe.so

printf '%s\n' '=== embedded CPython probe ==='
./embed_probe

printf '%s\n' '=== stable-ABI extension probe ==='
PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}" "$PYTHON" -c '
import pyx_capi_probe

result = pyx_capi_probe.add(2, 3)
print(f"abi3_tag={pyx_capi_probe.tag}")
print(f"abi3_add_2_3={result}")
if pyx_capi_probe.tag != "abi3" or result != 5:
    raise SystemExit("unexpected abi3 probe result")
'

printf '%s\n' '=== undefined CPython symbols in pyx_capi_probe.so ==='
if command -v "$NM" >/dev/null 2>&1; then
    UNDEFINED=$(
        "$NM" -D --undefined-only pyx_capi_probe.so \
            | awk '{print $NF}' \
            | grep '^Py' \
            | LC_ALL=C sort -u \
            || true
    )
    if [[ -n "$UNDEFINED" ]]; then
        printf '%s\n' "$UNDEFINED"
    fi
    COUNT=$(printf '%s\n' "$UNDEFINED" | awk 'NF { count++ } END { print count + 0 }')
    printf 'undefined_py_symbols=%s\n' "$COUNT"
else
    printf 'undefined_py_symbols=unavailable\n'
fi
