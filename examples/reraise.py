def reraiser():
    try:
        raise ValueError("original")
    except ValueError:
        raise


try:
    reraiser()
except ValueError as e:
    print("caught", e)

print("abc" * 2, 2 * "xy", True * "z")
