data = b"Hello, PyX!\n"
print(len(data))
print(data[0], data[-1])
print(data[0:5])
print(list(data[0:3]))
print(data.decode().strip())
print("hé".encode("utf-8"))
print(b"a,b,c".split(b","))
print(b"-".join([b"a", b"b"]))
print(b"Hello".startswith(b"He"), b"abc".hex())
keys = {b"key": 1}
print(keys[b"key"])
print(b"\x00\xff")
print(bytes([65, 66]), bytes(2), bytes("x", "utf-8"))
