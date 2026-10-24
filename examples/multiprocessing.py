import multiprocessing


channel = multiprocessing.Queue()


def child(out):
    out.put("hello")
    out.put([1, 2, 3])


process = multiprocessing.Process(target=child, args=(channel,))
process.start()
print(channel.get(), channel.get())
process.join()
print(process.exitcode())
print(multiprocessing.get_start_method())
