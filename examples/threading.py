import threading


lock = threading.Lock()
results = []


def worker(lock, results, n):
    i = 0
    while i < n:
        lock.acquire()
        results.append(i)
        lock.release()
        i += 1


threads = []
for _ in range(3):
    thread = threading.Thread(target=worker, args=(lock, results, 100))
    threads.append(thread)
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()

print(len(results))
print(threading.active_count() >= 1)
