def with_return():
    try:
        print("body")
        return 1
    finally:
        print("finally")


print(with_return())


def suppress():
    try:
        raise ValueError("suppressed")
    finally:
        print("cleanup")
        return 7


print(suppress())


def nested():
    try:
        try:
            return 3
        finally:
            print("inner-finally")
    finally:
        print("outer-finally")


print(nested())

try:
    print("ok")
except ValueError:
    print("bad")
else:
    print("else")
finally:
    print("fin")

try:
    try:
        print("inner")
    except ValueError:
        print("no")
    else:
        print("else-inner")
    finally:
        print("fin-inner")
except TypeError:
    print("nope")

i = 0
while i < 5:
    try:
        i += 1
        if i == 2:
            continue
        if i == 4:
            break
        print("loop", i)
    finally:
        print("finally", i)
print("after", i)


def propagate():
    try:
        raise ValueError("boom")
    finally:
        print("propagate-finally")


try:
    propagate()
except ValueError as e:
    print("caught", e)


def else_raises():
    try:
        print("try-else")
    except ValueError:
        print("not here")
    else:
        raise TypeError("from else")


try:
    else_raises()
except TypeError as e:
    print("caught-else", e)

try:
    raise KeyError("bare")
except:
    print("bare caught")

while True:
    try:
        raise ValueError("suppressed by break")
    finally:
        break
print("after-break")

try:
    try:
        raise ValueError("first")
    finally:
        raise TypeError("second")
except TypeError as e:
    print("second wins", e)
