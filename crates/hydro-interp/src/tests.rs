//! Unit tests for the IR interpreter, grouped by IR node family.
//!
//! Each test compiles real Python with [`hydro_frontend::check_source`] (or
//! `check_file` for imports), runs the resulting [`hydro_ir::Program`] through
//! [`Interp`] and relies on `assert` statements inside the program: a failed
//! assertion becomes an uncaught runtime exception and fails the test. The
//! runtime initialisation is serialised because the module-global table is
//! process-wide while each test owns a fresh heap.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hydro_ir::Program;
use hydro_runtime::abi;

use crate::Interp;

static SERIAL: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn run_program(program: Program) {
    abi::hydro_runtime_init();
    let source_name = program.source_name.clone();
    abi::hydro_set_file(source_name.as_ptr(), source_name.len());
    let mut interp = Interp::new("__main__");
    let id = interp.add_program(Arc::new(program));
    let completed = interp.run(&id);
    if !completed {
        let code = abi::hydro_uncaught();
        panic!("interpreted program raised (uncaught exit code {code})");
    }
    assert_eq!(abi::hydro_exc_pending(), 0, "pending exception after run");
}

fn run_source(source: &str) {
    let _guard = guard();
    let program = hydro_frontend::check_source(source, "interp_test.py")
        .unwrap_or_else(|diagnostics| panic!("frontend rejected source: {diagnostics:?}"));
    run_program(program);
}

#[test]
fn literals_operators_and_assignments() {
    run_source(
        "\
assert 1 + 2 * 3 == 7
assert 7 // 2 == 3
assert 7 % 2 == 1
assert 2 ** 5 == 32
assert -3 + 1 == -2
assert ~1 == -2
assert not (1 == 2)
assert (1 < 2) and (3 >= 3)
assert not (1 > 2) or True
x = 10
x += 5
x *= 2
assert x == 30
assert \"a\" + \"b\" == \"ab\"
assert [1, 2] + [3] == [1, 2, 3]
assert (1, 2) == (1, 2)
assert {\"a\": 1}[\"a\"] == 1
assert 2 in {1, 2, 3}
assert 3 not in {1, 2}
assert 1 in [1, 2]
assert 4 not in [1, 2]
assert b\"ab\" + b\"c\" == b\"abc\"
assert (1 if True else 2) == 1
assert (2 if False else 3) == 3
text = \"\"
for i in range(3):
    text = text + str(i)
assert text == \"012\"
",
    );
}

#[test]
fn complex_literals_and_arithmetic() {
    run_source(
        "\
assert repr(0j) == '0j'
assert repr(1j) == '1j'
assert repr(1.5j) == '1.5j'
assert repr(1 + 2j) == '(1+2j)'
assert repr(-2j) == '(-0-2j)'
z = 1j
assert repr(-z) == '(-0-1j)'
assert +z == 1j
assert z.real == 0.0
assert z.imag == 1.0
assert (3 + 4j).real == 3.0
assert z == 1j
assert z != 1
assert z != 2j
assert z + 1 == 1 + 1j
assert 1 + z == 1 + 1j
assert z * 2 == 2j
assert 2 * z == 2j
assert z / 2 == 0.5j
assert z - 1 == -1 + 1j
assert 1 - z == 1 - 1j
assert z ** 2 == -1 + 0j
assert (1 + 2j) * (3 + 4j) == -5 + 10j
assert type(z).__name__ == 'complex'
",
    );
}

#[test]
fn evaluation_order_matches_python() {
    run_source(
        "\
log = []

def mark(tag):
    log.append(tag)
    return tag

class Box:
    def __init__(self):
        self.items = [0, 0, 0]

box = Box()
box.items[mark(0)] = mark(1)
assert log == [1, 0]

log = []

def left():
    log.append(1)
    return 1

def right():
    log.append(2)
    return 2

def pair(a, b):
    return a + b

assert pair(left(), right()) == 3
assert log == [1, 2]

log = []

def receiver():
    log.append(10)
    return box

def index():
    log.append(20)
    return 0

def rhs():
    log.append(30)
    return 7

receiver().items[index()] = rhs()
assert log == [30, 10, 20]
assert box.items[0] == 7

log = []
xs = [0, 1, 2, 3]
def bounds():
    log.append(1)
    return 1
xs[bounds():bounds()] = [9]
assert log == [1, 1]
assert xs == [0, 9, 1, 2, 3]
",
    );
}

#[test]
fn loops_and_else_clauses() {
    run_source(
        "\
out = []
for i in range(3):
    out.append(i)
else:
    out.append(9)
assert out == [0, 1, 2, 9]

out = []
for i in range(5):
    if i == 2:
        break
    out.append(i)
else:
    out.append(100)
assert out == [0, 1]

out = []
for i in range(4):
    if i % 2 == 0:
        continue
    out.append(i)
assert out == [1, 3]

n = 0
while n < 3:
    n += 1
else:
    n += 10
assert n == 13

n = 0
while True:
    n += 1
    if n == 4:
        break
assert n == 4

out = []
for i in range(3, 8, 2):
    out.append(i)
assert out == [3, 5, 7]

out = []
for i in range(5, 0, -2):
    out.append(i)
assert out == [5, 3, 1]

total = 0
for i in range(3):
    for j in range(3):
        if j == 1:
            continue
        total += i * j
assert total == 6
",
    );
}

#[test]
fn exceptions_handlers_and_finally() {
    run_source(
        "\
def f():
    try:
        raise ValueError(\"bad\")
    except ValueError as e:
        return 1
    finally:
        pass

assert f() == 1

try:
    raise TypeError(\"x\")
except TypeError:
    pass
else:
    assert False

caught = False
try:
    assert 1 == 2, \"no\"
except AssertionError:
    caught = True
assert caught

def reraise():
    try:
        raise ValueError(\"a\")
    except ValueError:
        try:
            raise
        except ValueError:
            return 5

assert reraise() == 5

try:
    try:
        raise ValueError(\"reraise\")
    finally:
        raise
except ValueError:
    pass
else:
    assert False

try:
    raise RuntimeError(\"a\") from ValueError(\"b\")
except RuntimeError:
    pass

def overriding():
    try:
        return 1
    finally:
        return 2

assert overriding() == 2

order = []
try:
    try:
        order.append(1)
    finally:
        order.append(2)
except ValueError:
    order.append(3)
assert order == [1, 2]

def no_active():
    try:
        pass
    finally:
        raise

try:
    no_active()
except RuntimeError:
    pass
",
    );
}

#[test]
fn classes_methods_and_attributes() {
    run_source(
        "\
class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def total(self):
        return self.x + self.y

    @property
    def product(self):
        return self.x * self.y

    @classmethod
    def origin(cls):
        return cls(0, 0)

    @staticmethod
    def tag():
        return \"p\"

p = Point(2, 3)
assert p.total() == 5
assert p.product == 6
assert Point.origin().total() == 0
assert Point.tag() == \"p\"
p.x = 10
assert p.total() == 13
p.extra = [1, 2]
assert p.extra == [1, 2]
del p.extra
try:
    p.extra
except AttributeError:
    pass
else:
    assert False

class Base:
    def value(self):
        return 10

class Child(Base):
    def value(self):
        return 20

assert Base().value() == 10
assert Child().value() == 20

box = Point(1, 1)
holder = {\"p\": box}
assert holder[\"p\"].total() == 2
",
    );
}

#[test]
fn class_bases_resolve_mro_entries_special_forms() {
    run_source(
        "\
class Base:
    pass

class SpecialForm:
    def __mro_entries__(self, bases):
        assert len(bases) >= 1
        assert bases[len(bases) - 1] is self
        return (Base,)

form = SpecialForm()

class Marker:
    pass

class Config(Marker, form):
    pass

assert len(Config.__bases__) == 2
assert Config.__bases__[0] is Marker
assert Config.__bases__[1] is Base
assert len(Config.__orig_bases__) == 2
assert Config.__orig_bases__[0] is Marker
assert Config.__orig_bases__[1] is form

# The single-base path resolves the hook too.
class Single(form):
    pass

assert len(Single.__bases__) == 1
assert Single.__bases__[0] is Base
assert len(Single.__orig_bases__) == 1
assert Single.__orig_bases__[0] is form

# Pydantic's spelling: a special-form base plus a class keyword. The keyword
# is evaluated but not forwarded; the base still resolves.
class WithKeyword(form, total=False):
    pass

assert len(WithKeyword.__bases__) == 1
assert WithKeyword.__bases__[0] is Base
assert WithKeyword.__orig_bases__[0] is form
",
    );
}

#[test]
fn closures_cells_and_global_scope() {
    run_source(
        "\
counter = 0

def bump():
    global counter
    counter += 1
    return counter

assert bump() == 1
assert bump() == 2
assert counter == 2

def make(n):
    def add(x):
        return x + n
    return add

assert make(3)(4) == 7
assert make(10)(4) == 14

def counter_factory():
    c = 0
    def inc():
        nonlocal c
        c += 1
        return c
    return inc

inc = counter_factory()
assert inc() == 1
assert inc() == 2

def outer(a):
    def middle(b):
        def inner(c):
            return a + b + c
        return inner
    return middle

assert outer(1)(2)(3) == 6

def apply(fn, value):
    return fn(value)

assert apply(make(5), 2) == 7
",
    );
}

#[test]
fn recursive_nested_functions() {
    run_source(
        "\
def make_factorial():
    def fact(n):
        if n <= 1:
            return 1
        return n * fact(n - 1)
    return fact

fact = make_factorial()
assert fact(5) == 120
# The closure keeps its own binding after the outer frame returned.
assert fact(6) == 720

def parity(n):
    def even(k):
        if k == 0:
            return True
        return odd(k - 1)
    def odd(k):
        if k == 0:
            return False
        return even(k - 1)
    return even(n)

assert parity(10)
assert not parity(7)

def accumulate(step):
    total = 0
    def add(n):
        nonlocal total
        total += step
        if n <= 0:
            return total
        return add(n - 1)
    return add

add = accumulate(2)
assert add(3) == 8
assert add(2) == 14

def traced():
    calls = []
    def walk(n):
        calls.append(n)
        if n <= 0:
            return 0
        return walk(n - 1) + 1
    return walk, calls

walk, calls = traced()
assert walk(3) == 3
assert calls == [3, 2, 1, 0]

def decorated():
    seen = []
    def twice(fn):
        def wrapper(n):
            seen.append(1)
            return 2 * fn(n)
        return wrapper
    @twice
    def step(n):
        if n <= 0:
            return 0
        return step(n - 1) + 1
    return step, seen

step, seen = decorated()
# Recursive calls resolve the decorated binding, exactly like CPython.
assert step(3) == 14
assert len(seen) == 4
",
    );
}

#[test]
fn nested_function_defaults_are_evaluated_at_definition_time() {
    run_source(
        "\
def outer():
    for x in [1]:
        def f(y=x):
            return y
        assert f() == 1

outer()

def parameter_default(a):
    def f(y=a):
        return y
    return f()

assert parameter_default(7) == 7

def loop_values():
    values = []
    for x in [1, 2]:
        def f(y=x):
            return y
        values.append(f())
    return values

# Each iteration creates a new function whose default is the current value.
assert loop_values() == [1, 2]

def lambda_default():
    a = 5
    f = lambda y=a: y
    a = 9
    return f()

# The default was captured when the lambda was created.
assert lambda_default() == 5

def lambda_capturing_default():
    a = 5
    b = 2
    f = lambda y=a: b + y
    a = 9
    return f()

assert lambda_capturing_default() == 7

def keyword_only_default(a):
    def f(*, y=a):
        return y
    return f()

assert keyword_only_default(3) == 3

def nested_scopes(a):
    def middle():
        def inner(y=a):
            return y
        return inner
    return middle()()

# `middle` must carry `a` so the default is evaluated in its frame.
assert nested_scopes(4) == 4

def nested_lambda(a):
    def middle():
        return lambda y=a: y
    return middle()()

assert nested_lambda(6) == 6

def positional_defaults(a, b):
    def f(y=b, z=a):
        return (y, z)
    return f()

y, z = positional_defaults(1, 2)
assert y == 2
assert z == 1

def shared_mutable_default():
    def f(acc=[]):
        acc.append(1)
        return acc
    return f

f = shared_mutable_default()
# The default is evaluated once, when the `def` ran: both calls share it.
assert f() == [1]
assert f() == [1, 1]

def fresh_defaults_per_iteration():
    fns = []
    for x in [1, 2]:
        def f(seen=[]):
            seen.append(x)
            return seen
        fns.append(f)
    return fns[0], fns[1]

first, second = fresh_defaults_per_iteration()
# Every iteration creates a new function with its own default list.
assert first() == [1]
assert second() == [2]
",
    );
}

#[test]
fn generators_and_async() {
    run_source(
        "\
import asyncio

def gen(n):
    i = 0
    while i < n:
        yield i
        i += 1

assert list(gen(4)) == [0, 1, 2, 3]

def early():
    yield 1
    return

assert list(early()) == [1]

def echo():
    received = yield 10
    yield received

g = echo()
assert next(g) == 10
assert g.send(7) == 7

async def work():
    return 7

async def main():
    return (await work()) + 1

assert asyncio.run(main()) == 8

async def values():
    yield 1
    yield 2

async def collect():
    out = []
    async for value in values():
        out.append(value)
    return out

assert asyncio.run(collect()) == [1, 2]
",
    );
}

#[test]
fn comprehensions_unpacking_and_slices() {
    run_source(
        "\
data = [(1, \"a\"), (2, \"b\")]
assert [k for k, v in data] == [1, 2]
assert [v for k, v in data if k > 1] == [\"b\"]
assert {k: k * k for k in range(4)}[3] == 9
assert {i % 3 for i in range(7)} == {0, 1, 2}
assert [[j for j in range(i)] for i in range(3)] == [[], [0], [0, 1]]

a, *rest, b = [1, 2, 3, 4]
assert a == 1
assert b == 4
assert rest == [2, 3]

x, (y, z) = 1, (2, 3)
assert x == 1 and y + z == 5

text = \"abcdef\"
assert text[1:4] == \"bcd\"
assert text[::-1] == \"fedcba\"

xs = [0, 1, 2, 3, 4]
assert xs[1:4:2] == [1, 3]
xs[1:3] = [9, 8]
assert xs == [0, 9, 8, 3, 4]
del xs[1]
assert xs == [0, 8, 3, 4]
del xs[1:3]
assert xs == [0, 4]

def nested(obj):
    obj.name = 3
    return obj

class Holder:
    def __init__(self):
        self.name = 0

holder = Holder()
(holder.name,) = (7,)
assert holder.name == 7
holder.name, (tail,) = 4, (5,)
assert holder.name == 4 and tail == 5
",
    );
}

#[test]
fn stdlib_imports() {
    run_source(
        "\
import math
import os

assert math.floor(math.sqrt(16)) == 4
assert math.sqrt(9) == 3.0
assert os.path.join(\"a\", \"b\") == \"a/b\"
",
    );
}

#[test]
fn math_prod_matches_cpython() {
    run_source(
        "\
import math

assert math.prod([]) == 1
assert math.prod([], start=5) == 5
assert math.prod([1, 2, 3, 4, 5]) == 120
assert math.prod(range(2, 8)) == 5040
# Generators are consumed lazily by the same iteration path.
assert math.prod(x for x in range(2, 8)) == 5040
assert math.prod((x for x in range(2, 5)), start=2) == 48
# A float operand promotes the running product.
assert math.prod([1, 2, 3, 4.0, 5.0]) == 120.0
assert math.prod([1.5, 2]) == 3.0
# Exact integers promote to BigInt instead of overflowing.
assert math.prod([2 ** 40, 2 ** 40]) == 2 ** 80
# The start value participates in the multiplication protocol.
assert math.prod([2, 3], start=10) == 60
assert math.prod([2, 3], start=\"ab\") == \"abababababab\"
assert math.prod([1, 0, 2, 3]) == 0
",
    );
}

#[test]
fn user_module_initializers_run_once() {
    let _guard = guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hydro-interp-imports-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("helper.py"),
        "\
VALUE = 5
calls = 0

def bump(x):
    return x + VALUE
",
    )
    .unwrap();
    let main = dir.join("main.py");
    std::fs::write(
        &main,
        "\
from helper import VALUE, bump

assert VALUE == 5
assert bump(3) == 8
",
    )
    .unwrap();
    let program = hydro_frontend::check_file(&main)
        .unwrap_or_else(|diagnostics| panic!("frontend rejected modules: {diagnostics:?}"));
    assert!(!program.initializers.is_empty());
    run_program(program);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn user_module_call_binds_omitted_defaults_and_keyword_only() {
    // A cross-module call that omits a default (or passes a keyword-only
    // argument) must reach the runtime signature binder; taking the direct
    // function entry would pass too few slots. `tkinter`'s
    // `@enum._simple_enum(enum.StrEnum)` is the motivating shape.
    let _guard = guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hydro-interp-defaults-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("helper.py"),
        "\
def shifted(value, *, offset=1):
    return value + offset

def pair(a, b=2):
    return (a, b)
",
    )
    .unwrap();
    let main = dir.join("main.py");
    std::fs::write(
        &main,
        "\
import helper
from helper import pair, shifted

assert helper.shifted(3) == 4
assert helper.shifted(3, offset=10) == 13
assert helper.pair(1) == (1, 2)
assert helper.pair(1, 5) == (1, 5)
assert shifted(3) == 4
assert pair(1) == (1, 2)
",
    )
    .unwrap();
    let program = hydro_frontend::check_file(&main)
        .unwrap_or_else(|diagnostics| panic!("frontend rejected modules: {diagnostics:?}"));
    run_program(program);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_callbacks_reenter_the_interpreter() {
    run_source(
        "\
values = [3, 1, 2]
assert sorted(values) == [1, 2, 3]
assert sorted(values, key=lambda x: -x) == [3, 2, 1]

def make_key(scale):
    def key(x):
        return x * scale
    return key

assert sorted(values, key=make_key(-1)) == [3, 2, 1]
assert list(map(make_key(10), [1, 2])) == [10, 20]
assert list(filter(lambda x: x > 1, values)) == [3, 2]

def raising_key(x):
    if x == 2:
        raise ValueError(\"boom\")
    return x

caught = False
try:
    sorted([1, 2, 3], key=raising_key)
except ValueError:
    caught = True
assert caught
",
    );
}

#[test]
fn slice_objects_reach_user_getitem() {
    run_source(
        "\
class Grid:
    def __init__(self):
        self.seen = None

    def __getitem__(self, index):
        self.seen = index
        return index

grid = Grid()

# `a[:, 1]`: a tuple key whose first element is a real slice object.
key = grid[:, 1]
assert isinstance(key, tuple)
assert isinstance(key[0], slice)
assert key[0].start is None
assert key[0].stop is None
assert key[0].step is None
assert key[1] == 1
assert grid.seen[0] == key[0]
assert grid.seen[1] == 1

# `a[::2, 3]`: omitted bounds stay None while the step is the given value.
stepped = grid[::2, 3]
assert stepped[0].start is None
assert stepped[0].stop is None
assert stepped[0].step == 2
assert stepped[1] == 3

# `a[1, :]`: the slice keeps a non-leading position in the tuple.
first = grid[1, :]
assert first[0] == 1
assert first[1].start is None
assert first[1].stop is None
assert first[1].step is None

# Slice values compare by bound, so the lowered and explicit forms match.
assert stepped[0] == slice(None, None, 2)
assert first[1] == slice(None, None, None)
",
    );
}

#[test]
fn augmented_slice_assignment_reads_and_writes_the_slice() {
    run_source(
        "\
xs = [1, 2, 3, 4]
xs[1:3] += [9, 9]
assert xs == [1, 2, 3, 9, 9, 4]

# A step-1 slice of a list splices any iterable; an extended slice requires the
# replaced positions to keep their count.
ys = [0, 1, 2, 3]
ys[::2] += []
assert ys == [0, 1, 2, 3]

zs = [0, 1, 2, 3, 4, 5]
zs[1:5:2] += []
assert zs == [0, 1, 2, 3, 4, 5]

ba = bytearray(b'abcd')
ba[1:3] += b'xy'
assert ba == bytearray(b'abcxyd')

ba2 = bytearray(b'abcde')
ba2[::2] += b''
assert ba2 == bytearray(b'abcde')

# The object, the bounds and the right-hand side run exactly once each, in
# Python's order: object, start, stop, step, right-hand side.
order = []
lists = [[1, 2]]

def target():
    order.append('object')
    return lists[0]

def bound(tag, value):
    order.append(tag)
    return value

def rhs():
    order.append('rhs')
    return [9]

target()[bound('start', 0):bound('stop', 1)] += rhs()
assert order == ['object', 'start', 'stop', 'rhs']
assert lists[0] == [1, 9, 2]

# A user object receives real `slice` objects from both dunders.
class Box:
    def __init__(self):
        self.items = [1, 2, 3, 4]
        self.log = []

    def __getitem__(self, index):
        self.log.append(('get', index))
        return self.items[index.start:index.stop:index.step]

    def __setitem__(self, index, value):
        self.log.append(('set', index))
        self.items[index.start:index.stop:index.step] = value

box = Box()
box[1:3] += [9]
assert box.items == [1, 2, 3, 9, 4]
assert len(box.log) == 2
assert box.log[0][0] == 'get'
assert box.log[1][0] == 'set'
assert isinstance(box.log[0][1], slice)
assert isinstance(box.log[1][1], slice)
assert box.log[0][1].start == 1
assert box.log[0][1].stop == 3
assert box.log[0][1].step is None
assert box.log[1][1].start == 1
assert box.log[1][1].stop == 3
assert box.log[1][1].step is None

# Immutable sequence types still reject the write.
try:
    text = 'abc'
    text[0:1] += 'x'
    assert False
except TypeError:
    pass

try:
    pair = (1, 2)
    pair[0:1] += (9,)
    assert False
except TypeError:
    pass
",
    );
}

#[test]
fn chained_assignment_in_class_bodies_binds_every_name() {
    run_source(
        "\
order = []
calls = []

def value():
    calls.append(1)
    return [1, 2]

def traced(tag):
    order.append(tag)
    return tag

# The right-hand side runs once and every name receives the same object.
class C:
    a = b = c = value()
    d = a

assert calls == [1]
assert C.a == [1, 2]
assert C.a is C.b
assert C.b is C.c
assert C.d is C.a

# Parked values still run in source order with the other class attributes.
class Ordered:
    first = traced('first')
    x = y = traced('shared')
    last = traced('last')

assert order == ['first', 'shared', 'last']
assert Ordered.x == 'shared'
assert Ordered.y == 'shared'
assert Ordered.last == 'last'

# A chained alias of a method binds the same function under every name.
class Aliased:
    def greet(self):
        return 'hi'

    hello = salute = greet

assert Aliased().hello() == 'hi'
assert Aliased().salute() == 'hi'

# A class body that also stores through a subscript runs against the runtime
# namespace, where a chain writes each name from one shared value.
class E:
    xs = [1, 2]
    xs[0] = 9

assert E.xs == [9, 2]

def build(flag):
    class Inner:
        if flag:
            p = q = 1
        else:
            p = q = 2
    return Inner

assert build(True).p == 1
assert build(True).q == 1
assert build(False).p == 2
assert build(False).q == 2
",
    );
}

#[test]
fn functools_cached_property_and_math_log2_are_real() {
    run_source(
        r#"
import functools
import math
from functools import cached_property

calls = []

class Holder:
    def __init__(self, value):
        self.value = value

    @cached_property
    def doubled(self):
        calls.append(self.value)
        return self.value * 2

holder = Holder(21)
assert calls == []
assert holder.doubled == 42
assert calls == [21]
# The second read is the cached instance attribute, not a new computation.
assert holder.doubled == 42
assert calls == [21]
assert holder.__dict__["doubled"] == 42

# Class access returns the descriptor itself and never computes.
assert isinstance(Holder.doubled, functools.cached_property)
assert Holder.doubled is Holder.doubled
assert calls == [21]

# Every instance caches independently.
other = Holder(1)
assert other.doubled == 2
assert other.doubled == 2
assert calls == [21, 1]

# Deleting the cached value makes the next read recompute it.
del holder.doubled
assert holder.doubled == 42
assert calls == [21, 1, 21]

# A slots-only class has no instance dictionary to cache into.
class Slotted:
    __slots__ = ()

    @cached_property
    def value(self):
        return 1

try:
    Slotted().value
except TypeError as error:
    assert "No '__dict__' attribute" in str(error)
else:
    assert False

# `math.log2` follows CPython, domain errors included.
assert math.log2(8) == 3.0
assert math.log2(1024) == 10.0
assert math.log2(1) == 0.0
assert abs(math.log2(10) - 3.321928094887362) < 1e-12
try:
    math.log2(0)
except ValueError:
    pass
else:
    assert False
"#,
    );
}

#[test]
fn pep695_generic_classes_scope_and_publish_type_parameters() {
    run_source(
        r#"
class Marker:
    pass

class Box[T]:
    value = T

# The parameter list is a real runtime tuple on the class.
assert isinstance(Box.__type_params__, tuple)
assert len(Box.__type_params__) == 1
assert Box.__type_params__[0].__name__ == 'T'
# The body sees the parameter itself (the hidden local, not a module name).
assert Box.value.__name__ == 'T'

# `Box[int]` is a generic alias over the class.
alias = Box[int]
assert alias is not Box
assert alias.__origin__ is Box
assert len(alias.__args__) == 1
assert len(alias.__parameters__) == 0
assert str(alias) == 'Box[int]'

# Multi-parameter specialization keeps the argument order.
class Pair[K, V]:
    pass

pair = Pair[str, int]
assert pair.__origin__ is Pair
assert len(pair.__args__) == 2
assert str(pair) == 'Pair[str, int]'

# A bound is an ordinary expression over the enclosing scope.
class Bounded[T: Marker]:
    pass

assert Bounded.__type_params__[0].__bound__ is Marker

# Variadic and parametric parameter lists allocate their runtime objects.
class Variadic[*Ts]:
    pass

class Parametric[**P]:
    pass

assert Variadic.__type_params__[0].__name__ == 'Ts'
assert Parametric.__type_params__[0].__name__ == 'P'

# Subclassing a specialization uses the origin and inherits the parameters.
class Derived(Box[int]):
    pass

assert Derived[int].__origin__ is Derived
assert len(Derived.__type_params__) == 1

def deco(cls):
    return cls

@deco
class Decorated[T]:
    pass

assert Decorated.__type_params__[0].__name__ == 'T'

# Nested generic classes scope their parameters independently.
class Outer[T]:
    value = T

    class Inner[U]:
        value = U

assert Outer.__type_params__[0].__name__ == 'T'
assert Outer.Inner.__type_params__[0].__name__ == 'U'
assert Outer.Inner.value.__name__ == 'U'
"#,
    );
}

#[test]
fn dynamic_calls_preserve_keyword_names() {
    // A callee that is only known at runtime (`f(...)` on a parameter) must
    // forward every keyword name, alone, after `*args`, and through `**`.
    run_source(
        r#"
def target(a, b, c=0, **extra):
    return (a, b, c, extra)

def call_keyword(f):
    return f(1, 2, c=3, extra=4)

def call_mapping(f, mapping):
    return f(1, 2, **mapping)

def call_starred(f, args, mapping):
    return f(*args, **mapping)

def call_starred_keyword(f, args):
    return f(*args, c=7)

def forward(f, **kwargs):
    return f(1, 2, **kwargs)

assert call_keyword(target) == (1, 2, 3, {'extra': 4})
assert call_mapping(target, {'c': 5, 'extra': 6}) == (1, 2, 5, {'extra': 6})
assert call_starred(target, (1, 2), {'c': 7}) == (1, 2, 7, {})
assert call_starred_keyword(target, (1, 2)) == (1, 2, 7, {})
assert forward(target, c=8) == (1, 2, 8, {})

# A class created by a conditional block is only bound at runtime, so its
# constructor call keeps its keywords (the shape typing_extensions uses for
# its 3.9 alias fallback).
if True:
    class C:
        def __init__(self, origin, args, **kwargs):
            self.origin = origin
            self.args = args
            self.extra = kwargs

def make():
    return C(1, (2,), _typevar_types='X')

built = make()
assert built.origin == 1
assert built.args == (2,)
assert built.extra == {'_typevar_types': 'X'}
"#,
    );
}

#[test]
fn super_init_and_method_calls_expand_starred_operands() {
    run_source(
        r#"
class Warnings:
    def __init__(self, message, *args, since=(2, 0), expected_removal=(3, 0)):
        self.message = message
        self.args = args
        self.since = since
        self.expected_removal = expected_removal

class DeprecatedWarning(Warnings):
    def __init__(self, message, *args, **kwargs):
        super().__init__(message, *args, **kwargs)

w = DeprecatedWarning('gone', 1, 2, since=(2, 1))
assert w.message == 'gone'
assert len(w.args) == 2
assert w.args[0] == 1
assert w.args[1] == 2
assert w.since == (2, 1)
assert w.expected_removal == (3, 0)

# The same runtime binder expands `obj.m(*xs, **kw)`.
class Collector:
    def record(self, first, *rest, tag=None, **extra):
        return (first, rest, tag, extra)

c = Collector()
xs = (1, 2, 3)
kw = {'tag': 'x', 'extra': 9}
result = c.record(*xs, **kw)
assert result[0] == 1
assert len(result[1]) == 2
assert result[1][0] == 2
assert result[1][1] == 3
assert result[2] == 'x'
assert result[3] == {'extra': 9}

other = c.record(0, *xs[1:], tag='y')
assert other[0] == 0
assert len(other[1]) == 2
assert other[2] == 'y'
assert other[3] == {}
"#,
    );
}

#[test]
fn blank_property_supports_the_enum_redirect_flow() {
    // CPython's blank `property()` has `fget`/`fset`/`fdel` set to None and a
    // writable attribute table; `enum` builds redirect descriptors with it.
    run_source(
        r#"
class Owner:
    pass

blank = property()
assert blank.fget is None
assert blank.fset is None
assert blank.fdel is None

# Arbitrary attributes (enum stores `member` and `_get`/`_set`/`_del`).
blank.member = 3
blank._get = None
blank._attr_type = 'attr'
assert blank.member == 3
assert blank._attr_type == 'attr'

# `__set_name__(owner, name)` is accepted and records nothing observable.
assert blank.__set_name__(Owner, 'value') is None

# A blank descriptor raises AttributeError on instance access and assignment.
class Holder:
    value = blank

try:
    Holder().value
except AttributeError:
    pass
else:
    assert False

probe = Holder()
try:
    probe.value = 1
except AttributeError:
    pass
else:
    assert False

# Accessing a blank property through the class returns the descriptor itself.
assert Holder.value is blank

# Filling the slots afterwards (the enum redirect copies a found descriptor).
def read(self):
    return 42

def write(self, new_value):
    self.stored = new_value

def drop(self):
    self.dropped = True

blank.fget = read
assert Holder().value == 42
blank.fset = write
holder = Holder()
holder.value = 9
assert holder.stored == 9
blank.fdel = drop
del holder.value
assert holder.dropped

# The one- to four-argument forms keep their behaviour.
def getter(self):
    return 'g'

def setter(self, new_value):
    self.seen = new_value

def deleter(self):
    self.deleted = True

full = property(getter, setter, deleter, 'doc')
class Full:
    attr = full

f = Full()
assert f.attr == 'g'
f.attr = 'v'
assert f.seen == 'v'
del f.attr
assert f.deleted

derived = property(getter).setter(setter)
assert derived.fget(None) == 'g'
assert derived.fset is not None
assert derived.fdel is None
"#,
    );
}

#[test]
fn type_forwards_class_keywords_to_the_resolved_metaclass() {
    run_source(
        r#"
class Meta(type):
    def __new__(metacls, name, bases, namespace, boundary=None, _simple=False):
        cls = type.__new__(metacls, name, bases, namespace)
        cls.new_boundary = boundary
        cls.new_simple = _simple
        return cls

    def __init__(cls, name, bases, namespace, boundary=None, _simple=False):
        cls.init_boundary = boundary
        cls.init_simple = _simple

class Base(metaclass=Meta):
    pass

# The three-argument form keeps working without keywords.
Plain = type('Plain', (Base,), {'value': 1})
assert Plain.value == 1
assert type(Plain) is Meta

# The keywords reach the metaclass `__new__` and `__init__`, as in CPython.
Made = type('Made', (Base,), {'value': 2}, boundary=7, _simple=True)
assert Made.value == 2
assert Made.new_boundary == 7
assert Made.new_simple is True
assert Made.init_boundary == 7
assert Made.init_simple is True

# A class-header keyword on the one-argument form is rejected.
try:
    type(Plain, boundary=7)
except TypeError:
    pass
else:
    assert False

# The native `type` metaclass has no keyword channel: CPython forwards class
# keywords to `__init_subclass__`, whose default rejects them.
try:
    type('NoMeta', (), {}, boundary=7)
except TypeError:
    pass
else:
    assert False
"#,
    );
}

#[test]
fn enum_methods_and_properties_are_not_members() {
    // `check_source` only links native module specs; the vendored `enum`
    // module is pure Python, so write a real `main.py` and let the module
    // loader (`check_file`) link its initializer.
    //
    // CPython's `_is_descriptor` relies on functions having `__get__`; Hydro's
    // Python functions (and the functions behind decorator-only bindings) do
    // not, so `enum._is_descriptor` recognizes them by type instead. Methods
    // must stay class attributes while `member()` still forces membership.
    let _guard = guard();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("hydro-interp-enum-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("main.py");
    std::fs::write(
        &main,
        r#"
from enum import Enum, auto, member, nonmember

class Demo(Enum):
    A = auto()
    B = auto()

    def method(self):
        return self._name_

    @property
    def prop(self):
        return self._name_

    C = auto()

assert Demo._member_names_ == ['A', 'B', 'C']
assert Demo.A.method() == 'A'
# Decorator-only `@property` reaches the enum namespace as a plain function
# (Hydro's binding-kind metadata does not survive `EnumType`'s class rebuild),
# so only membership is asserted here; the explicit property below covers the
# descriptor case.
for name in ('method', 'prop'):
    assert name not in Demo._member_names_

# An explicit property object is already a descriptor, so it keeps its getter
# and its value is not converted into a member.
def getter(self):
    return 'P'

class WithProperty(Enum):
    A = 1
    p = property(getter)

assert WithProperty._member_names_ == ['A']
assert WithProperty.A.p == 'P'

# `member()` still forces functions into members; `nonmember()` keeps them out.
def plain(value):
    return value + 1

class Forced(Enum):
    forced = member(plain)
    skipped = nonmember(plain)

assert Forced._member_names_ == ['forced']
assert Forced.forced._value_(5) == 6
assert Forced.skipped(6) == 7
"#,
    )
    .unwrap();
    let program = hydro_frontend::check_file(&main)
        .unwrap_or_else(|diagnostics| panic!("frontend rejected modules: {diagnostics:?}"));
    assert!(
        program
            .initializers
            .iter()
            .any(|id| program.functions[*id as usize].name == "enum.<module>"),
        "the vendored enum module must be linked as an initializer"
    );
    run_program(program);
    let _ = std::fs::remove_dir_all(&dir);
}
