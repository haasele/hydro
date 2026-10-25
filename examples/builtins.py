r = range(5)
print(r, len(r), list(r), sum(r))
print(range(0, 10, 2), range(5, 0, -2))
print(sum(range(10)), sum(range(1, 10, 2)), sum(range(10), 100))
print(list(range(2, 8, 2)), list(range(5, 0, -2)))
print(list("abc"), list((1, 2)), list({"a": 1, "b": 2}))
print(len(list({3, 1, 2})), 3 in list({3, 1, 2}))
print(dict([(1, "a"), (2, "b")]))
print(dict(zip([1, 2], ["a", "b"])))

for i, v in enumerate(["a", "b"]):
    print(i, v)
for i, v in enumerate(["a", "b"], 10):
    print(i, v)
for a, b in zip([1, 2, 3], ["x", "y"]):
    print(a, b)

print(list(zip([1, 2], [3, 4])), list(enumerate(["x"])))
print(sum([1, 2, 3]), sum([1.5, 2]), sum([], 7))
print(5 in range(10), 10 in range(10), 1 in range(0, 10, 2))
print(len(range(0)), len(range(1)))

try:
    print(range(0, 5, 0))
except ValueError as e:
    print("zero", e)

try:
    print(sum(["a", "b"]))
except TypeError as e:
    print("sum", e)

print(ord("A"), ord("é"), chr(65), chr(233), chr(0x1F600))
print(chr(ord("z")))

try:
    print(ord("ab"))
except TypeError as e:
    print("ord", e)

try:
    print(chr(-1))
except ValueError as e:
    print("chr", e)

try:
    print(chr(0xD800))
except ValueError as e:
    print("chr", e)

if (n := 10) > 5:
    print("walrus", n)
print(n)

i = 0
while (i := i + 1) < 3:
    print("loop", i)
print("after", i)

print([y := v * 2 for v in [1, 2, 3]])
print(y)

calls = [0]


def probe(value):
    calls[0] = calls[0] + 1
    return value


print(1 < probe(2) < 3, calls[0])
print(3 < probe(2) < 5, calls[0])
print(1 < probe(2) < probe(3) < 5, calls[0])
print(0 <= i <= 1 < 2, i)
