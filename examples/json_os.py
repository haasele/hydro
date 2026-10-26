import json
import os

data = {"name": "Hydro", "n": 3, "ok": True, "items": [1, 2.5, None], "nested": {"x": "y"}}
text = json.dumps(data)
print(text)
back = json.loads(text)
print(back["name"], back["n"], back["ok"], back["items"][1], back["nested"]["x"])
print(json.loads("[1, \"a\", true, null, 2.5]"))
print(json.dumps("a\"b\nc"))
print(json.dumps([1, 2.0, False, None]))
print(json.loads("{\"a\": 1, \"a\": 2}")["a"])
print(json.dumps({"a": [1, {"b": "c"}]}))
print(json.loads("{\"u\": \"\\u00e9\\u20ac\"}")["u"])
print(json.dumps("é€"))

print(os.getpid() > 0, os.cpu_count() > 0, len(os.name) > 0, os.sep == "/")
print(len(os.getcwd()) > 0)
print(os.getenv("DEFINITELY_NOT_SET_HYD", "fallback"))
print(os.getenv("DEFINITELY_NOT_SET_HYD"))

try:
    json.loads("{")
except ValueError:
    print("bad")

try:
    json.dumps(lambda x: x)
except TypeError:
    print("dumps")

try:
    json.loads(5)
except TypeError:
    print("loads")
