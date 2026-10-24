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
