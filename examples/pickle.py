import pickle


data = {"a": [1, 2, 3], "b": ("x", True, None)}
encoded = pickle.dumps(data)
print(len(encoded) > 0)
print(pickle.loads(encoded))
