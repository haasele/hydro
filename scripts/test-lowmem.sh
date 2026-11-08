#!/usr/bin/env bash
# Low-memory, serialized, logged nextest runner.
#
# The client has been killed by overlapping/high-parallel test runs. This
# wrapper is the only supported local entry point for verification:
#   - serializes runs with flock (no overlapping cargo/nextest processes)
#   - caps rustc jobs and nextest test threads
#   - writes full output and /usr/bin/time -v metrics under .hydro/logs
#   - disables core dumps so a crash cannot fill the disk
#
# Usage:
#   scripts/test-lowmem.sh [nextest args...]
#   scripts/test-lowmem.sh -p hydro-runtime modules::codecs
#   HYDRO_TEST_LOG_DIR=/tmp/hydro-logs scripts/test-lowmem.sh -p hydro-frontend
set -euo pipefail

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export NEXTEST_TEST_THREADS="${NEXTEST_TEST_THREADS:-2}"
ulimit -c 0 2>/dev/null || true

LOG_DIR="${HYDRO_TEST_LOG_DIR:-$ROOT/.hydro/logs}"
mkdir -p "$LOG_DIR"
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOG="$LOG_DIR/test-$STAMP-$$.log"
METRICS="$LOG.time"

exec 9>"$LOG_DIR/.lock"
if ! flock -n 9; then
    echo "another low-memory test run is already active ($LOG_DIR/.lock)" >&2
    exit 75
fi

{
    echo "== hydro low-memory test run =="
    echo "time:    $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "root:    $ROOT"
    echo "git:     $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "args:    $*"
    echo "jobs:    CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS NEXTEST_TEST_THREADS=$NEXTEST_TEST_THREADS"
    echo "memory:"
    free -m
    echo
} | tee "$LOG"

CMD=(cargo nextest run --no-fail-fast --test-threads "$NEXTEST_TEST_THREADS" "$@")
TIMEOUT_CMD=()
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_CMD=(timeout --foreground --signal=INT "${HYDRO_TEST_TIMEOUT:-300}s")
fi
RAW="$LOG.raw"
MIN_AVAIL="${HYDRO_MIN_AVAIL_MB:-1500}"
: > "$METRICS"
RUN=()
if command -v setsid >/dev/null 2>&1; then
    RUN=(setsid)
fi
set +e
"${RUN[@]}" "${TIMEOUT_CMD[@]}" "${CMD[@]}" >"$RAW" 2>&1 &
PID=$!
(
    while kill -0 "$PID" 2>/dev/null; do
        avail=$(awk '/MemAvailable/ {print int($2/1024)}' /proc/meminfo)
        rss=$(awk '/VmRSS/ {print int($2/1024)}' "/proc/$PID/status" 2>/dev/null || echo 0)
        printf '%s avail_mb=%s cargo_rss_mb=%s\n' \
            "$(date -u +%H:%M:%SZ)" "$avail" "$rss" >> "$METRICS"
        if (( avail < MIN_AVAIL )); then
            printf 'watchdog: avail_mb=%s below %s; terminating test group\n' \
                "$avail" "$MIN_AVAIL" >> "$LOG"
            pkill -TERM -f 'cargo nextest run' 2>/dev/null || true
            kill -TERM "$PID" 2>/dev/null || true
            break
        fi
        sleep 2
    done
) &
WATCH=$!
wait "$PID"
STATUS=$?
wait "$WATCH" 2>/dev/null || true
cat "$RAW" >> "$LOG"
rm -f "$RAW"
set -e

{
    echo
    echo "== end of run =="
    echo "exit:    $STATUS"
    if [[ -f "$METRICS" ]]; then
        echo "memory samples (last 5, min available ${MIN_AVAIL}MB):"
        tail -5 "$METRICS"
    fi
    free -m
    echo "log:     $LOG"
} | tee -a "$LOG"

exit "$STATUS"
