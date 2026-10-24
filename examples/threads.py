import _thread
import time


def worker(lock, results, counter, done, n):
    i = 0
    while i < n:
        lock.acquire()
        counter[0] = counter[0] + 1
        results.append(i)
        lock.release()
        i += 1
    lock.acquire()
    done[0] = done[0] + 1
    lock.release()


lock = _thread.allocate_lock()
results = []
counter = [0]
done = [0]
t1 = _thread.start_new_thread(worker, (lock, results, counter, done, 1000))
t2 = _thread.start_new_thread(worker, (lock, results, counter, done, 1000))
while True:
    lock.acquire()
    finished = done[0] == 2
    lock.release()
    if finished:
        break
    time.sleep(0.001)
print(len(results), counter[0])


def sleeper(lock, done):
    time.sleep(0.001)
    lock.acquire()
    done[0] = done[0] + 1
    lock.release()


done[0] = 0
t3 = _thread.start_new_thread(sleeper, (lock, done))
while True:
    lock.acquire()
    finished = done[0] == 1
    lock.release()
    if finished:
        break
    time.sleep(0.001)
print("threads done")

lock2 = _thread.allocate_lock()
print(lock2.locked())
print(lock2.acquire())
print(lock2.locked())
print(lock2.acquire(False))
lock2.release()
print(lock2.locked())
print(_thread.get_ident() >= 0)
