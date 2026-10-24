class C:
    def __init__(self, x):
        self.x = x

    def get(self):
        return self.x

class D(C):
    def double(self):
        return self.x * 2

c = C(4)
print(c.get(), c.x)
c.x = 7
print(c.get())
d = D(5)
print(d.get(), d.double())
try:
    print(c.nope)
except AttributeError:
    print("missing")
