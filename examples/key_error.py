try:
    raise KeyError("k")
except KeyError as e:
    print("key", e)
