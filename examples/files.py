import io
import os


path = "/tmp/hydro-files-" + str(os.getpid()) + ".txt"
with open(path, "w", encoding="utf-8") as handle:
    handle.write("alpha\n")
    handle.write("beta\n")

with io.open(path, "r", encoding="utf-8") as handle:
    print(handle.readline().strip())
    print(handle.read().strip())

with open(path, encoding="utf-8") as handle:
    for line in handle:
        print(line.strip())

try:
    open(path + ".missing", encoding="utf-8")
except OSError:
    print("missing")

os.remove(path)
