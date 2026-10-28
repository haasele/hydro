import multiprocessing


def work(n):
    total = 0
    i = 0
    while i < n:
        total += i
        i += 1
    return total


def produce(out, n):
    out.put(work(n))


if __name__ == "__main__":
    # Two fork processes, one result queue each; the sum is order-independent.
    left = multiprocessing.Queue()
    right = multiprocessing.Queue()
    first = multiprocessing.Process(target=produce, args=(left, 250000))
    second = multiprocessing.Process(target=produce, args=(right, 250000))
    first.start()
    second.start()
    first.join()
    second.join()
    print(left.get() + right.get())
