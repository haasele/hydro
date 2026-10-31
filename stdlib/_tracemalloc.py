# Hydro's pure-Python `_tracemalloc` compatibility module.
#
# CPython implements this module as a C extension that hooks the PyMem
# allocators. Hydro's heap exposes no per-block allocation hook (the ABI host
# reports every `PyTraceMalloc_Track` call as "not tracing"), and it publishes
# no counter for the number or size of live Python objects. `gc.get_stats()`
# only reports cumulative collector counters, not allocation sizes.
#
# This module therefore implements the *state* half of the CPython API
# truthfully and leaves the allocation table empty:
#
#   * `start()` / `stop()` / `is_tracing()` maintain the tracing flag;
#   * `get_traceback_limit()` / `set_traceback_limit()` maintain the frame
#     limit (default 1, allowed range 1..65535, as in CPython);
#   * `get_traced_memory()` reports `(0, 0)` because zero memory blocks are
#     being traced -- it is not a process-wide RSS figure;
#   * `get_tracemalloc_memory()` reports 0 because this module stores no
#     trace table;
#   * `_get_traces()` returns an empty list and `_get_object_traceback()`
#     returns `None`, which is CPython's own answer for untraced objects;
#   * `reset_peak()` / `clear_traces()` operate on the empty table and are
#     therefore no-ops.
#
# No allocation is fabricated. Public code should import `tracemalloc`, which
# wraps these primitives in `Snapshot`, `Traceback` and `Filter` objects.
#
# CPython's C extension has no `set_traceback_limit()`, no module-level
# `take_snapshot()`, no `get_object_traceback()` alias and no symbolic domain
# constants. They are provided here because Hydro's requested surface lists
# them: `set_traceback_limit()` is the validated setter behind `start(nframe)`,
# `take_snapshot()` returns the same raw list as `_get_traces()`,
# `get_object_traceback()` is an alias of `_get_object_traceback()`, and the
# `DOMAIN_*` names spell out the documented numeric domain 0.

__all__ = [
    'start', 'stop', 'is_tracing', 'reset_peak',
    'get_traced_memory', 'get_tracemalloc_memory',
    'get_traceback_limit', 'set_traceback_limit',
    'take_snapshot', 'get_object_traceback',
    '_get_traces', '_get_object_traceback', '_get_traceback_limit',
    'clear_traces',
    'DOMAIN_PYTHON', 'DOMAIN_DEFAULT',
]

# CPython documents domain 0 as the address space of Python's own memory
# allocations; extensions that call PyTraceMalloc_Track pick their own integer
# domains. CPython publishes no symbolic module-level names for them, so Hydro
# keeps the values explicit here instead of hard-coding 0 in filters.
DOMAIN_PYTHON = 0
DOMAIN_DEFAULT = 0

_MIN_TRACEBACK_FRAMES = 1
_MAX_TRACEBACK_FRAMES = 65535

_tracing = False
_traceback_limit = 1


def _check_nframe(nframe):
    """Validate a traceback frame count like CPython's C implementation."""
    if not isinstance(nframe, int):
        raise TypeError("'%s' object cannot be interpreted as an integer"
                        % type(nframe).__name__)
    if nframe < _MIN_TRACEBACK_FRAMES or nframe > _MAX_TRACEBACK_FRAMES:
        raise ValueError("the number of frames must be in range [%d; %d]"
                         % (_MIN_TRACEBACK_FRAMES, _MAX_TRACEBACK_FRAMES))


def start(nframe=1, /):
    """Start tracing Python memory allocations.

    Hydro only maintains the tracing state: it never observes allocations.
    The frame limit is updated only when tracing actually starts, matching
    CPython (a second `start()` call is a validated no-op).
    """
    global _tracing, _traceback_limit
    _check_nframe(nframe)
    if _tracing:
        return None
    _traceback_limit = int(nframe)
    _tracing = True
    return None


def stop():
    """Stop tracing and clear the (always empty) trace table."""
    global _tracing
    _tracing = False
    return None


def is_tracing():
    """Return True after `start()` and before `stop()`."""
    return _tracing


def reset_peak():
    """Reset the peak traced size.

    No block is ever traced, so the peak is always zero and this is a no-op.
    """
    return None


def get_traced_memory():
    """Return `(current, peak)` bytes of traced memory blocks.

    Hydro's runtime does not hook allocations, so no block is traced and both
    values are 0. This is the truthful answer for the tracemalloc table, not
    a measurement of the process heap.
    """
    return (0, 0)


def get_tracemalloc_memory():
    """Return the memory used by tracemalloc's trace table (0 bytes)."""
    return 0


def get_traceback_limit():
    """Return the maximum number of frames stored per trace."""
    return _traceback_limit


def set_traceback_limit(nframe):
    """Set the maximum number of frames stored per trace.

    CPython only sets this limit through `start(nframe)`. Hydro exposes this
    helper as well because the module-level API requested for this emulation
    includes it; the limit is validated exactly like `start()` and does not
    resurrect old traces.
    """
    global _traceback_limit
    _check_nframe(nframe)
    _traceback_limit = int(nframe)
    return None


def _get_traceback_limit():
    """Compatibility alias for `get_traceback_limit()`."""
    return _traceback_limit


def clear_traces():
    """Drop every trace. There are none, so this is a no-op."""
    return None


def _get_traces():
    """Return the raw trace list.

    CPython returns a list of `(domain, size, traceback, total_nframe)`
    tuples. Hydro has no traced blocks, so the list is empty.
    """
    return []


def _get_object_traceback(obj, /):
    """Return the traceback where `obj` was allocated, or None.

    Hydro does not record allocation tracebacks, so an object is never
    "traced": `None` is the documented answer for that case.
    """
    return None


def get_object_traceback(obj):
    """Compatibility alias for `_get_object_traceback()`."""
    return None


def take_snapshot():
    """Return the raw traces as a list.

    This mirrors `_get_traces()`, matching the low-level nature of this module.
    The public `tracemalloc.take_snapshot()` wraps the result in a `Snapshot`;
    the trace table is empty here. Keeping this function self-contained also
    avoids a lazy dependency on the higher-level module.
    """
    return _get_traces()
