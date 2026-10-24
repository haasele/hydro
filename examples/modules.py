import math
import time as t
import sys
from functools import reduce
from operator import add, mul
from math import sqrt, pi as PI

print(math.sqrt(9))
print(sqrt(16), PI > 3)
print(math.floor(2.7), math.ceil(2.1), math.trunc(-2.7), math.fabs(-3.5))
print(math.pow(2, 3), math.exp(0), math.log(math.e))
print(math.sin(0), math.cos(0))
print(math.inf > 1, math.nan != math.nan)
print(add(2, 3), mul(4, 5), add("a", "b"))
print(reduce(add, [1, 2, 3, 4]))
print(reduce(mul, [1, 2, 3, 4]))
print(reduce(add, [], 10))
print(reduce(add, [1, 2], 10))


def total(xs):
    return reduce(add, xs)


print(total([1, 2, 3, 4]))
print(len(sys.argv) >= 1, len(sys.version) > 0)
print(sys.platform == "linux")
print(sys.maxsize > 0)

start = t.perf_counter()
t.sleep(0.001)
print(t.perf_counter() >= start)
print(t.time() > 0, t.time_ns() > 0)
