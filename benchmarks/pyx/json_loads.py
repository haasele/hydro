import json

text = json.dumps([{"id": i, "v": i * 2} for i in range(1000)])
data = json.loads(text)
print(len(data), data[999]["v"])
