def make(x):
    def add(y):
        return x + y
    return add

f = make(2)
g = make(10)
print(f(3), g(3))

def outer(a):
    def middle(b):
        def inner(c):
            return a + b + c
        return inner
    return middle

print(outer(1)(2)(3))
