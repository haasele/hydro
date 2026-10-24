import math

total = 0
for i in range(50000):
    total += math.floor(math.sqrt(i))
print(total)
