t = (1, 2, 3)
print(t, len(t), t[0], t[-1])
print(t[1:], t[::-1], (1,))
print((), (1,), (1, 2) == (1, 2), (1, 2) == [1, 2])
print((1, 2) + (3, 4), (1, 2) * 3, 2 * (0,))

total = 0
for x in t:
    total += x
print(total)

a, b, c = t
print(a, b, c)


def pair():
    return (4, 5)


x, y = pair()
print(x, y)

d = {"a": 1, "b": 2}
for k, v in d.items():
    print(k, v)

d[(1, 2)] = "tuple-key"
print(d[(1, 2)], len(d))

print((1, 2).count(1), (1, 2, 1).index(2))
print((1, 2) == (1, 2), (1, 2) != (1, 3))
print(str((1, "a", None)))

try:
    a, b = (1, 2, 3)
except ValueError as e:
    print("unpack", e)

try:
    a, b, c = (1, 2)
except ValueError as e:
    print("unpack", e)

try:
    (1, 2)[5]
except IndexError as e:
    print("index", e)

try:
    d[(1, [2])] = 3
except TypeError as e:
    print("hash", e)
