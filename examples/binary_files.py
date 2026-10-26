import os


path = "/tmp/hydro-binary-" + str(os.getpid()) + ".bin"
with open(path, "wb") as handle:
    handle.write(b"\x00A\nB\n")

with open(path, "rb") as handle:
    data = handle.read()
    print(len(data))
    print(data[0], data[-1])

with open(path, "rb") as handle:
    for line in handle:
        print(line)

os.remove(path)
