add = lambda a, b: a + b
total = 0
for i in range(100000):
    total = add(total, i)
print(total)
