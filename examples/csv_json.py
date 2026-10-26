import csv
import json
import os


base = "/tmp/hydro-csv-" + str(os.getpid())
path = base + ".csv"
dict_path = base + "-dict.csv"
json_path = base + ".json"

rows = [["name", "qty", "note"], ["a", 1, "x,y"], ["b", 2, 'say "hi"']]
with open(path, "w", newline="") as handle:
    writer = csv.writer(handle)
    writer.writerows(rows)

with open(path, newline="") as handle:
    for row in csv.reader(handle):
        print(row)

with open(path, newline="") as handle:
    for row in csv.DictReader(handle):
        print(row["name"], row["qty"])

with open(dict_path, "w", newline="") as handle:
    writer = csv.DictWriter(handle, fieldnames=["id", "value"])
    writer.writeheader()
    writer.writerow({"id": 1, "value": "x"})

with open(dict_path, newline="") as handle:
    print(list(csv.DictReader(handle)))

data = {"a": [1, 2], "b": True}
with open(json_path, "w", encoding="utf-8") as handle:
    json.dump(data, handle)
with open(json_path, encoding="utf-8") as handle:
    print(json.load(handle))

os.remove(path)
os.remove(dict_path)
os.remove(json_path)
