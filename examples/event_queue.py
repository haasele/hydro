import queue
import threading


items = queue.Queue()
done = threading.Event()


def producer(items, done):
    i = 0
    while i < 10:
        items.put(i)
        i += 1
    done.set()


thread = threading.Thread(target=producer, args=(items, done))
thread.start()
print(items.get(), items.get())
thread.join()
print(done.wait(), items.qsize(), items.empty())
