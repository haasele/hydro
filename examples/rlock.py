import threading


lock = threading.RLock()
lock.acquire()
lock.acquire()
print(lock.locked())
lock.release()
print(lock.locked())
lock.release()
print(lock.locked())
