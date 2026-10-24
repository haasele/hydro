print([i * 2 for i in range(5)])
print([i for i in range(10) if i % 3 == 0])
print([i + j for i in range(3) for j in range(2)])
print([[j for j in range(i)] for i in range(3)])
print({i: i * i for i in range(4)})
s = {i % 3 for i in range(7)}
print(len(s), 0 in s, 1 in s, 2 in s)
print(sum(i for i in range(10)))
print("".join(str(i) for i in range(4)))
data = [(1, "a"), (2, "b")]
print([k for k, v in data])
print({k: v for k, v in data})
print([v for k, v in data if k > 1])
n = 3
print([n + i for i in range(3)])
i = 100
print([i for i in range(3)], i)
print([i for i in range(3) if i > 0 if i < 2])
print([(i, j) for i in range(2) for j in range(2) if i != j])
print(list(i for i in range(3)))
print([i for i, v in enumerate(["a", "b"])])
