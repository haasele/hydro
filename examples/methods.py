s = "  Hello World  "
print(s.strip())
print(s.strip().upper())
print("a,b,c".split(","))
print("a b  c".split())
print("-".join(["a", "b", "c"]))
print("banana".replace("a", "o"))
print("banana".replace("a", "o", 2))
print("hello".startswith("he"), "hello".endswith("lo"))
print("banana".find("na"), "banana".count("na"))
print("123".isdigit(), "abc".isalpha(), "a1".isalnum(), " \t".isspace())

xs = [1, 2]
xs.append(3)
xs.extend([4, 5])
print(xs, len(xs))
print(xs.pop(), xs)
xs.insert(0, 0)
xs.remove(2)
xs.reverse()
print(xs, xs.count(1), xs.index(3))
ys = xs.copy()
ys.append(99)
print(xs, ys)

d = {"a": 1}
print(d.get("a"), d.get("z"), d.get("z", 0))
d.update({"b": 2})
d["c"] = 3
print(d)
for k in d.keys():
    print(k)
for v in d.values():
    print(v)
print(d.pop("a"), d)
print(d.pop("z", -1))


def dynamic_attr(x):
    return x.nope()


def dynamic_arity(x):
    return x.append()


try:
    dynamic_attr("x" or 1)
except AttributeError as e:
    print("attr", e)

try:
    dynamic_arity([1] or 0)
except TypeError as e:
    print("type", e)

try:
    [].pop()
except IndexError as e:
    print("index", e)

try:
    {"a": 1}.pop("z")
except KeyError as e:
    print("key", e)

try:
    "a".join([1, 2])
except TypeError as e:
    print("join", e)
