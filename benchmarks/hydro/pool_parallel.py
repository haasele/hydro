import multiprocessing


def work(n):
    total = 0
    i = 0
    while i < n:
        total += i
        i += 1
    return total


if __name__ == "__main__":
    # Scaling pair: same chunks as pool_serial.py, four workers.
    pool = multiprocessing.Pool(4)
    results = pool.map(work, [250000, 250000, 250000, 250000])
    print(sum(results))
    pool.close()
    pool.join()
