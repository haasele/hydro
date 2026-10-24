import _thread
import time


def worker(out, done, lock, wid):
    total = 0
    i = 0
    while i < 200000:
        total += i
        i += 1
    lock.acquire()
    out[wid] = total
    done[0] = done[0] + 1
    lock.release()


lock = _thread.allocate_lock()
out = [0, 0, 0, 0]
done = [0]
for wid in range(4):
    _thread.start_new_thread(worker, (out, done, lock, wid))

while True:
    lock.acquire()
    finished = done[0] == 4
    lock.release()
    if finished:
        break
    time.sleep(0.001)

total = 0
for value in out:
    total += value
print(total)
