# Hydro's pure-Python `tracemalloc` compatibility module.
#
# The public object model (`Snapshot`, `Traceback`, `Trace`, `Statistic`,
# `Filter`, `DomainFilter`) follows CPython 3.14's `Lib/tracemalloc.py`.  The
# underlying `_tracemalloc` extension does not exist in Hydro, so the vendored
# `_tracemalloc` module (stdlib/_tracemalloc.py) provides its state API.
#
# Hydro's heap does not expose per-block allocation hooks or a live allocated
# byte counter: `gc.get_stats()` reports cumulative collector counts only.  As
# a consequence:
#
#   * `get_traced_memory()` returns `(0, 0)` while tracing, because zero
#     blocks are traced (this is not process RSS);
#   * `_get_traces()` is empty, so `take_snapshot()` returns an empty Snapshot
#     with the real traceback limit, and `statistics()`/`compare_to()` return
#     empty lists;
#   * `get_object_traceback()` returns `None`, CPython's documented result for
#     an object whose allocation was not traced.
#
# No allocation or traceback is fabricated.  All filtering, grouping and
# formatting code is fully functional for snapshots constructed from raw
# trace tuples (the format CPython's `_get_traces()` returns), so the object
# model keeps the CPython semantics even though live tracing is empty.  When
# `linecache` is unavailable, `Traceback.format()` keeps the `File ...` lines
# and omits only the source-text lines.
#
# Unlike CPython's class definitions, these classes do not use `__slots__` and
# do not inherit from `collections.abc.Sequence`.  Hydro does not support
# subclassing native container types, so the sequence protocol is implemented
# directly (`__len__`, `__getitem__`, `__iter__`, `__contains__`); the
# observable CPython behavior is unchanged.  `Frame` and `Traceback` provide
# the comparisons that `functools.total_ordering` would otherwise install.
#
# Hydro's built-in `hash()` hashes user-defined instances by identity instead
# of calling `__hash__`, so equal `Frame`/`Traceback`/`Statistic` objects do not
# necessarily share a hash in this runtime.  Snapshot grouping keys its
# internal maps by primitive tuples and strings, which keeps `statistics()`
# and `compare_to()` correct; the public `__hash__` methods stay in place for
# CPython-compatible source code.
#
# Hydro's `pickle` implements its own HYD1 format and cannot serialize class
# instances, so `Snapshot.dump()` writes a versioned plain mapping and
# `Snapshot.load()` rebuilds a Snapshot from it (see the methods).

import fnmatch
from os.path import normcase as _normcase

try:
    import linecache
except ImportError:  # pragma: no cover - only when the CPython tree is absent
    linecache = None

from _tracemalloc import (
    start,
    stop,
    is_tracing,
    reset_peak,
    get_traced_memory,
    get_tracemalloc_memory,
    get_traceback_limit,
    set_traceback_limit,
    clear_traces,
    _get_traces,
    _get_object_traceback,
    _get_traceback_limit,
    DOMAIN_PYTHON,
    DOMAIN_DEFAULT,
)

# Hydro's `pickle` implementation speaks its own HYD1 format and cannot
# serialize user-defined class instances, while CPython's snapshot files are
# pickled `Snapshot` objects. `Snapshot.dump()` therefore writes a versioned
# plain mapping (strings, ints, lists and tuples only) and `load()` rebuilds a
# Snapshot from it. The two file formats are not interchangeable.
_SNAPSHOT_FORMAT = 'hydro.tracemalloc.snapshot'
_SNAPSHOT_VERSION = 1


def _is_slice(index):
    # Hydro does not expose the `slice` type as a builtin name, although the
    # runtime does produce slice objects for `x[a:b]`.  Their class name is
    # stable, so inspect that instead of `isinstance(index, slice)`.
    return type(index).__name__ == 'slice'


def _format_size(size, sign):
    for unit in ('B', 'KiB', 'MiB', 'GiB', 'TiB'):
        if abs(size) < 100 and unit != 'B':
            # 3 digits (xx.x UNIT)
            if sign:
                return "%+.1f %s" % (size, unit)
            else:
                return "%.1f %s" % (size, unit)
        if abs(size) < 10 * 1024 or unit == 'TiB':
            # 4 or 5 digits (xxxx UNIT)
            if sign:
                return "%+.0f %s" % (size, unit)
            else:
                return "%.0f %s" % (size, unit)
        size /= 1024


class Statistic:
    """
    Statistic difference on memory allocations between two Snapshot instance.
    """

    def __init__(self, traceback, size, count):
        self.traceback = traceback
        self.size = size
        self.count = count

    def __hash__(self):
        return hash((self.traceback, self.size, self.count))

    def __eq__(self, other):
        if not isinstance(other, Statistic):
            return NotImplemented
        return (self.traceback == other.traceback
                and self.size == other.size
                and self.count == other.count)

    def __str__(self):
        text = ("%s: size=%s, count=%i"
                 % (self.traceback,
                    _format_size(self.size, False),
                    self.count))
        if self.count:
            average = self.size / self.count
            text += ", average=%s" % _format_size(average, False)
        return text

    def __repr__(self):
        return ('<Statistic traceback=%r size=%i count=%i>'
                % (self.traceback, self.size, self.count))

    def _sort_key(self):
        return (self.size, self.count, self.traceback)


class StatisticDiff:
    """
    Statistic difference on memory allocations between an old and a new
    Snapshot instance.
    """

    def __init__(self, traceback, size, size_diff, count, count_diff):
        self.traceback = traceback
        self.size = size
        self.size_diff = size_diff
        self.count = count
        self.count_diff = count_diff

    def __hash__(self):
        return hash((self.traceback, self.size, self.size_diff,
                     self.count, self.count_diff))

    def __eq__(self, other):
        if not isinstance(other, StatisticDiff):
            return NotImplemented
        return (self.traceback == other.traceback
                and self.size == other.size
                and self.size_diff == other.size_diff
                and self.count == other.count
                and self.count_diff == other.count_diff)

    def __str__(self):
        text = ("%s: size=%s (%s), count=%i (%+i)"
                % (self.traceback,
                   _format_size(self.size, False),
                   _format_size(self.size_diff, True),
                   self.count,
                   self.count_diff))
        if self.count:
            average = self.size / self.count
            text += ", average=%s" % _format_size(average, False)
        return text

    def __repr__(self):
        return ('<StatisticDiff traceback=%r size=%i (%+i) count=%i (%+i)>'
                % (self.traceback, self.size, self.size_diff,
                   self.count, self.count_diff))

    def _sort_key(self):
        return (abs(self.size_diff), self.size,
                abs(self.count_diff), self.count,
                self.traceback)


def _compare_grouped_stats(old_group, new_group):
    statistics = []
    for key, stat in new_group.items():
        previous = old_group.pop(key, None)
        if previous is not None:
            stat = StatisticDiff(stat.traceback,
                                 stat.size, stat.size - previous.size,
                                 stat.count, stat.count - previous.count)
        else:
            stat = StatisticDiff(stat.traceback,
                                 stat.size, stat.size,
                                 stat.count, stat.count)
        statistics.append(stat)

    for key, stat in old_group.items():
        stat = StatisticDiff(stat.traceback, 0, -stat.size, 0, -stat.count)
        statistics.append(stat)
    return statistics


class Frame:
    """
    Frame of a traceback.
    """

    def __init__(self, frame):
        # frame is a tuple: (filename: str, lineno: int)
        self._frame = frame

    @property
    def filename(self):
        return self._frame[0]

    @property
    def lineno(self):
        return self._frame[1]

    def __eq__(self, other):
        if not isinstance(other, Frame):
            return NotImplemented
        return (self._frame == other._frame)

    def __lt__(self, other):
        if not isinstance(other, Frame):
            return NotImplemented
        return (self._frame < other._frame)

    def __le__(self, other):
        if not isinstance(other, Frame):
            return NotImplemented
        return (self._frame <= other._frame)

    def __gt__(self, other):
        if not isinstance(other, Frame):
            return NotImplemented
        return (self._frame > other._frame)

    def __ge__(self, other):
        if not isinstance(other, Frame):
            return NotImplemented
        return (self._frame >= other._frame)

    def __hash__(self):
        return hash(self._frame)

    def __str__(self):
        return "%s:%s" % (self.filename, self.lineno)

    def __repr__(self):
        return "<Frame filename=%r lineno=%r>" % (self.filename, self.lineno)


class Traceback:
    """
    Sequence of Frame instances sorted from the oldest frame
    to the most recent frame.
    """

    def __init__(self, frames, total_nframe=None):
        # frames is a tuple of frame tuples: see Frame constructor for the
        # format of a frame tuple; it is reversed, because _tracemalloc
        # returns frames sorted from most recent to oldest, but the
        # Python API expects oldest to most recent
        self._frames = tuple(frames)[::-1]
        self._total_nframe = total_nframe

    @property
    def total_nframe(self):
        return self._total_nframe

    def __len__(self):
        return len(self._frames)

    def __getitem__(self, index):
        if _is_slice(index):
            frames = self._frames[index.start:index.stop:index.step]
            return tuple([Frame(trace) for trace in frames])
        else:
            return Frame(self._frames[index])

    def __iter__(self):
        return iter([Frame(trace) for trace in self._frames])

    def __contains__(self, frame):
        return frame._frame in self._frames

    def __hash__(self):
        return hash(self._frames)

    def __eq__(self, other):
        if not isinstance(other, Traceback):
            return NotImplemented
        return (self._frames == other._frames)

    def __lt__(self, other):
        if not isinstance(other, Traceback):
            return NotImplemented
        return (self._frames < other._frames)

    def __le__(self, other):
        if not isinstance(other, Traceback):
            return NotImplemented
        return (self._frames <= other._frames)

    def __gt__(self, other):
        if not isinstance(other, Traceback):
            return NotImplemented
        return (self._frames > other._frames)

    def __ge__(self, other):
        if not isinstance(other, Traceback):
            return NotImplemented
        return (self._frames >= other._frames)

    def __str__(self):
        return str(self[0])

    def __repr__(self):
        s = "<Traceback %s" % (tuple(self),)
        if self._total_nframe is None:
            s += ">"
        else:
            s += " total_nframe=%s>" % self.total_nframe
        return s

    def format(self, limit=None, most_recent_first=False):
        lines = []
        if limit is not None:
            if limit > 0:
                frame_slice = self[-limit:]
            else:
                frame_slice = self[:limit]
        else:
            frame_slice = self

        if most_recent_first:
            frame_slice = frame_slice[::-1]
        for frame in frame_slice:
            lines.append('  File "%s", line %s'
                         % (frame.filename, frame.lineno))
            if linecache is None:
                continue
            line = linecache.getline(frame.filename, frame.lineno).strip()
            if line:
                lines.append('    %s' % line)
        return lines


def get_object_traceback(obj):
    """
    Get the traceback where the Python object *obj* was allocated.
    Return a Traceback instance.

    Return None if the tracemalloc module is not tracing memory allocations or
    did not trace the allocation of the object.  Hydro does not record
    allocation tracebacks, so this always returns None.
    """
    frames = _get_object_traceback(obj)
    if frames is not None:
        return Traceback(frames)
    else:
        return None


class Trace:
    """
    Trace of a memory block.
    """

    def __init__(self, trace):
        # trace is a tuple: (domain: int, size: int, traceback: tuple).
        # See Traceback constructor for the format of the traceback tuple.
        self._trace = trace

    @property
    def domain(self):
        return self._trace[0]

    @property
    def size(self):
        return self._trace[1]

    @property
    def traceback(self):
        return Traceback(*self._trace[2:])

    def __eq__(self, other):
        if not isinstance(other, Trace):
            return NotImplemented
        return (self._trace == other._trace)

    def __hash__(self):
        return hash(self._trace)

    def __str__(self):
        return "%s: %s" % (self.traceback, _format_size(self.size, False))

    def __repr__(self):
        return ("<Trace domain=%s size=%s, traceback=%r>"
                % (self.domain, _format_size(self.size, False), self.traceback))


class _Traces:
    def __init__(self, traces):
        # traces is a tuple of trace tuples: see Trace constructor
        self._traces = traces

    def __len__(self):
        return len(self._traces)

    def __getitem__(self, index):
        if _is_slice(index):
            traces = self._traces[index.start:index.stop:index.step]
            return tuple([Trace(trace) for trace in traces])
        else:
            return Trace(self._traces[index])

    def __iter__(self):
        return iter([Trace(trace) for trace in self._traces])

    def __contains__(self, trace):
        return trace._trace in self._traces

    def __eq__(self, other):
        if not isinstance(other, _Traces):
            return NotImplemented
        return (self._traces == other._traces)

    def __repr__(self):
        return "<Traces len=%s>" % len(self)


def _normalize_filename(filename):
    filename = _normcase(filename)
    if filename.endswith('.pyc'):
        filename = filename[:-1]
    return filename


class BaseFilter:
    def __init__(self, inclusive):
        self.inclusive = inclusive

    def _match(self, trace):
        raise NotImplementedError


class Filter(BaseFilter):
    def __init__(self, inclusive, filename_pattern,
                 lineno=None, all_frames=False, domain=None):
        super().__init__(inclusive)
        self.inclusive = inclusive
        self._filename_pattern = _normalize_filename(filename_pattern)
        self.lineno = lineno
        self.all_frames = all_frames
        self.domain = domain

    @property
    def filename_pattern(self):
        return self._filename_pattern

    def _match_frame_impl(self, filename, lineno):
        filename = _normalize_filename(filename)
        if not fnmatch.fnmatch(filename, self._filename_pattern):
            return False
        if self.lineno is None:
            return True
        else:
            return (lineno == self.lineno)

    def _match_frame(self, filename, lineno):
        return self._match_frame_impl(filename, lineno) ^ (not self.inclusive)

    def _match_traceback(self, traceback):
        if self.all_frames:
            for filename, lineno in traceback:
                if self._match_frame_impl(filename, lineno):
                    return self.inclusive
            return (not self.inclusive)
        else:
            filename, lineno = traceback[0]
            return self._match_frame(filename, lineno)

    def _match(self, trace):
        domain, size, traceback, total_nframe = trace
        res = self._match_traceback(traceback)
        if self.domain is not None:
            if self.inclusive:
                return res and (domain == self.domain)
            else:
                return res or (domain != self.domain)
        return res


class DomainFilter(BaseFilter):
    def __init__(self, inclusive, domain):
        super().__init__(inclusive)
        self._domain = domain

    @property
    def domain(self):
        return self._domain

    def _match(self, trace):
        domain, size, traceback, total_nframe = trace
        return (domain == self.domain) ^ (not self.inclusive)


class Snapshot:
    """
    Snapshot of traces of memory blocks allocated by Python.
    """

    def __init__(self, traces, traceback_limit):
        # traces is a tuple of trace tuples: see _Traces constructor for
        # the exact format
        self.traces = _Traces(traces)
        self.traceback_limit = traceback_limit

    def dump(self, filename):
        """
        Write the snapshot into a file.

        Hydro's pickle cannot serialize class instances, so the file holds a
        versioned plain mapping instead of a pickled Snapshot; `load()` turns
        it back into a Snapshot. See the module docstring for the limitation.
        """
        import pickle
        payload = {
            'format': _SNAPSHOT_FORMAT,
            'version': _SNAPSHOT_VERSION,
            'traceback_limit': self.traceback_limit,
            'traces': list(self.traces._traces),
        }
        with open(filename, "wb") as fp:
            pickle.dump(payload, fp, pickle.HIGHEST_PROTOCOL)

    @staticmethod
    def load(filename):
        """
        Load a snapshot from a file.

        Accepts the plain mapping written by `dump()`. CPython's pickled
        Snapshot files cannot be read because Hydro's unpickler implements its
        own HYD1 format; a foreign payload is reported as an invalid snapshot
        rather than returned as a wrong object.
        """
        import pickle
        with open(filename, "rb") as fp:
            payload = pickle.load(fp)
        if isinstance(payload, Snapshot):
            return payload
        if (isinstance(payload, dict)
                and payload.get('format') == _SNAPSHOT_FORMAT
                and payload.get('version') == _SNAPSHOT_VERSION):
            return Snapshot(payload['traces'], payload['traceback_limit'])
        raise ValueError("invalid tracemalloc snapshot file")

    def _filter_trace(self, include_filters, exclude_filters, trace):
        if include_filters:
            if not any([trace_filter._match(trace)
                        for trace_filter in include_filters]):
                return False
        if exclude_filters:
            if any([not trace_filter._match(trace)
                    for trace_filter in exclude_filters]):
                return False
        return True

    def filter_traces(self, filters):
        """
        Create a new Snapshot instance with a filtered traces sequence, filters
        is a list of Filter or DomainFilter instances.  If filters is an empty
        list, return a new Snapshot instance with a copy of the traces.
        """
        # CPython checks `isinstance(filters, Iterable)`. Hydro generators do
        # not expose `__iter__` as an attribute, so probe iterability with
        # `iter()` instead.
        try:
            iterator = iter(filters)
        except TypeError:
            raise TypeError("filters must be a list of filters, not %s"
                            % type(filters).__name__)
        if filters:
            include_filters = []
            exclude_filters = []
            for trace_filter in iterator:
                if trace_filter.inclusive:
                    include_filters.append(trace_filter)
                else:
                    exclude_filters.append(trace_filter)
            new_traces = [trace for trace in self.traces._traces
                          if self._filter_trace(include_filters,
                                                exclude_filters,
                                                trace)]
        else:
            new_traces = list(self.traces._traces)
        return Snapshot(new_traces, self.traceback_limit)

    def _group_by(self, key_type, cumulative):
        if key_type not in ('traceback', 'filename', 'lineno'):
            raise ValueError("unknown key_type: %r" % (key_type,))
        if cumulative and key_type not in ('lineno', 'filename'):
            raise ValueError("cumulative mode cannot by used "
                             "with key type %r" % (key_type,))

        stats = {}
        tracebacks = {}
        if not cumulative:
            for trace in self.traces._traces:
                domain, size, trace_traceback, total_nframe = trace
                if key_type == 'traceback':
                    key = trace_traceback
                elif key_type == 'lineno':
                    key = trace_traceback[:1]
                else: # key_type == 'filename':
                    key = ((trace_traceback[0][0], 0),)
                try:
                    traceback = tracebacks[key]
                except KeyError:
                    traceback = Traceback(key)
                    tracebacks[key] = traceback
                try:
                    stat = stats[key]
                    stat.size += size
                    stat.count += 1
                except KeyError:
                    stats[key] = Statistic(traceback, size, 1)
        else:
            # cumulative statistics
            for trace in self.traces._traces:
                domain, size, trace_traceback, total_nframe = trace
                for frame in trace_traceback:
                    if key_type == 'lineno':
                        key = (frame,)
                    else: # key_type == 'filename':
                        key = ((frame[0], 0),)
                    try:
                        traceback = tracebacks[key]
                    except KeyError:
                        traceback = Traceback(key)
                        tracebacks[key] = traceback
                    try:
                        stat = stats[key]
                        stat.size += size
                        stat.count += 1
                    except KeyError:
                        stats[key] = Statistic(traceback, size, 1)
        return stats

    def statistics(self, key_type, cumulative=False):
        """
        Group statistics by key_type. Return a sorted list of Statistic
        instances.
        """
        grouped = self._group_by(key_type, cumulative)
        statistics = list(grouped.values())
        statistics.sort(reverse=True, key=_statistic_sort_key)
        return statistics

    def compare_to(self, old_snapshot, key_type, cumulative=False):
        """
        Compute the differences with an old snapshot old_snapshot. Get
        statistics as a sorted list of StatisticDiff instances, grouped by
        group_by.
        """
        new_group = self._group_by(key_type, cumulative)
        old_group = old_snapshot._group_by(key_type, cumulative)
        statistics = _compare_grouped_stats(old_group, new_group)
        statistics.sort(reverse=True, key=_statistic_diff_sort_key)
        return statistics


def _statistic_sort_key(stat):
    return stat._sort_key()


def _statistic_diff_sort_key(stat):
    return stat._sort_key()


def take_snapshot():
    """
    Take a snapshot of traces of memory blocks allocated by Python.
    """
    if not is_tracing():
        raise RuntimeError("the tracemalloc module must be tracing memory "
                           "allocations to take a snapshot")
    traces = _get_traces()
    traceback_limit = get_traceback_limit()
    return Snapshot(traces, traceback_limit)
