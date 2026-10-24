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
