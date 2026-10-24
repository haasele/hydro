import os
import pathlib


base = pathlib.Path("/tmp/pyx-pathlib-example")
base.mkdir(False, True)
text_file = base / "a.txt"
binary_file = base / "b.bin"
text_file.write_text("hello\n")
binary_file.write_bytes(b"\x00A")
print(text_file.exists(), text_file.name, text_file.suffix)
print(base.joinpath("a.txt").read_text().strip())
print((base / "b.bin").read_bytes())
print(len(list(base.iterdir())))
print(len(list(base.glob("*.txt"))))
for entry in base.iterdir():
    print(entry.name)
os.remove(text_file)
os.remove(binary_file)
