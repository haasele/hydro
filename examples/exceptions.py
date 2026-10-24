def safe_div(a: int, b: int) -> int:
    try:
        return a // b
    except ZeroDivisionError as e:
        print("caught:", e)
        return 0


print(safe_div(10, 2))
print(safe_div(1, 0))

try:
    raise ValueError("boom")
except ValueError:
    print("raised ok")

try:
    raise ValueError("unhandled")
except TypeError:
    print("wrong handler")
