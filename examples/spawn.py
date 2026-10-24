import multiprocessing


def add_one(x):
    return x + 1


def child(channel, function):
    channel.put(function(41))


multiprocessing.set_start_method("spawn")
channel = multiprocessing.Queue()
process = multiprocessing.Process(target=child, args=(channel, add_one))
process.start()
print(channel.get())
process.join()
print(process.exitcode())
print(multiprocessing.get_start_method())
