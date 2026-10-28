import multiprocessing


def work(n):
    total = 0
    i = 0
    while i < n:
        total += i
        i += 1
    return total


if __name__ == "__main__":
    pool = multiprocessing.Pool(4)
    print(sum(pool.map(work, [50000, 50000, 50000, 50000])))
    pool.close()
    pool.join()
