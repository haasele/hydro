def f():
    try:
        return 5
    except ValueError:
        return 6
    print("BUG: code after try executed")


print(f())
