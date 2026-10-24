import _thread
import time


def worker(lock, results, done, n):
    i = 0
    while i < n:
        text = "x" + str(i)
        lock.acquire()
        results.append(text)
        lock.release()
        i += 1
    lock.acquire()
    done[0] = done[0] + 1
    lock.release()


lock = _thread.allocate_lock()
results = []
done = [0]
handles = []
for w in range(4):
    handles.append(_thread.start_new_thread(worker, (lock, results, done, 500)))

while True:
    lock.acquire()
    finished = done[0] == 4
    lock.release()
    if finished:
        break
    time.sleep(0.001)

print(len(results), done[0], len(results[0]) > 0, len(results[-1]) > 0)
