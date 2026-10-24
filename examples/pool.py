import multiprocessing


def square(x):
    return x * x


def add(a, b):
    return a + b


pool = multiprocessing.Pool(2)
print(pool.map(square, [1, 2, 3, 4]))
print(pool.apply(add, (20, 22)))
pool.close()
pool.join()
