def add(a, b):
    return a + b


f = add
print(f(2, 3))
print(list(map(lambda i: i * 2, range(5))))
print(list(filter(lambda i: i % 2 == 0, range(8))))
print(list(filter(None, [0, 1, "", "x", [], [1]])))
print(list(map(lambda i: i + 1, range(3))))


def apply_twice(fn, x):
    return fn(fn(x))


print(apply_twice(lambda x: x + 3, 1))

g = lambda a, b: a * b
print(g(4, 5))

funcs = [add, lambda x: x + 10]
print(funcs[0](7, 8), funcs[1](5))


def choose(flag):
    if flag:
        return lambda x: x + 1
    else:
        return lambda x: x - 1


print(choose(1)(10), choose(0)(10))


def call_with(fn, value):
    return fn(value)


print(call_with(lambda s: s + "!", "hi"))
