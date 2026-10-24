xs = {1, 2, 2, 3}
print(len(xs), 1 in xs, 4 in xs, 4 not in xs)
print(2 in [1, 2, 3], "b" in "abc", "z" not in "abc", "a" in {"a": 1}, 1 in (1, 2))

xs.add(4)
xs.add(2)
print(len(xs), 4 in xs)
xs.discard(9)
xs.discard(3)
print(len(xs), 3 in xs)

try:
    xs.remove(9)
except KeyError as e:
    print("remove", e)

ys = xs.copy()
ys.add(10)
print(len(xs), len(ys), 10 in xs, 10 in ys)

zs = xs.union({7, 8})
print(len(zs), 7 in zs, 1 in zs)

empty = set()
print(len(empty), 1 in empty, empty == set())
empty.add("x")
print(len(empty), "x" in empty)

from_list = set([1, 2, 2, 3])
print(len(from_list), 3 in from_list, 4 in from_list)

total = 0
for v in {2, 4, 6}:
    total += v
print(total)

print({1, 2} == {2, 1}, {1, 2} != {1, 3})

try:
    set([[1]])
except TypeError as e:
    print("hash", e)
