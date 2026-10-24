import platform
import random
import statistics


random.seed(42)
value = random.random()
print(value >= 0.0 and value < 1.0)
print(random.randint(5, 5))
print(random.randrange(0, 10, 2) >= 0)
rng = random.Random(seed=7)
print(rng.randint(1, 1))
print(random.choice("x"))
items = [3, 1, 2]
rng.shuffle(items)
print(len(items), items[0] in [1, 2, 3])
print(statistics.mean([1, 2, 3]))
print(statistics.median([3, 1, 2]))
print(platform.system() != "")
print(platform.python_implementation)
