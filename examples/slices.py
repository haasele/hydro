s = "abcdef"
print(s[1:4])
print(s[:3])
print(s[3:])
print(s[-3:])
print(s[::2])
print(s[::-1])
print(s[1:100])
print(s[-100:2])
print(len(s[1:3]))

xs = [0, 1, 2, 3, 4, 5]
print(xs[1:4])
print(xs[:2])
print(xs[4:])
print(xs[-2:])
print(xs[::2])
print(xs[::-1])
print(xs[4:1])
print(xs[1:4:2])
print(xs[::100])
print(xs[:100])
print(xs[-100:100])

try:
    print(xs[::0])
except ValueError as e:
    print("zero", e)

print("héllo"[1:3], "héllo"[::-1])

xs = [0, 1, 2, 3, 4, 5]
xs[1:3] = [9, 8]
print(xs)
xs[1:2] = [7, 6, 5]
print(xs)
xs[1:4] = []
print(xs)
xs[:2] = [4]
print(xs)
xs[-1:] = [8, 9]
print(xs)
xs[2:2] = [5]
print(xs)
xs[::2] = [0, 0, 0]
print(xs)
xs[::-1] = [1, 2, 3, 4, 5, 6]
print(xs)
xs[:] = xs
print(xs)
xs[1:2] = "ab"
print(xs)


def note(tag, value=None):
    print("eval", tag)
    return value


ys = [0, 1, 2]
ys[note("start", 1):note("stop", 2)] = note("value", [9])
print(ys)

try:
    xs[::2] = [1]
except ValueError as e:
    print("extended", e)

values = ["abc"]
s = values[0]
try:
    s[0:1] = "x"
except TypeError as e:
    print("immutable", e)
