import json

data = [{"id": i, "name": "item", "value": i * 2, "ok": True} for i in range(1000)]
text = json.dumps(data)
print(len(text))
