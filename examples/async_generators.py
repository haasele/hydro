"""Async generators, async for/with and asynchronous comprehensions.

Every construct here has the same observable behaviour under CPython 3.14.
"""

import asyncio


def sync_values():
    yield 1
    yield 2


async def ticker(n):
    for i in range(n):
        yield i


async def aiter_list(values):
    for value in values:
        yield value


async def collect(ait):
    out = []
    async for value in ait:
        out.append(value)
    return out


class Resource:
    def __init__(self, name):
        self.name = name
        self.entries = 0
        self.exits = 0

    async def __aenter__(self):
        self.entries += 1
        return self.name

    async def __aexit__(self, exc_type, exc, tb):
        self.exits += 1
        return False


class SuppressValueError:
    async def __aenter__(self):
        return None

    async def __aexit__(self, exc_type, exc, tb):
        return exc_type is ValueError


async def record(seen, label):
    seen.append(label)
    await asyncio.sleep(0)
    seen.append(label + '2')


async def grouped():
    seen = []
    async with asyncio.TaskGroup() as group:
        group.create_task(record(seen, 'a'))
        group.create_task(record(seen, 'b'))
    return seen


async def main():
    # A plain generator is unaffected by async generators in the same unit.
    print(list(sync_values()))

    # `async for` over an async generator.
    values = []
    async for value in ticker(3):
        values.append(value)
    print(values)

    # A coroutine can consume an async generator through a parameter.
    print(await collect(aiter_list([10, 20])))

    # Asynchronous comprehensions build eagerly and keep their result type.
    print([x * x async for x in aiter_list([1, 2, 3])])
    print(sorted({x % 2 async for x in aiter_list([1, 2, 3])}))
    print({x: x * 10 async for x in aiter_list([1, 2])})

    # An asynchronous generator expression stays lazy.
    lazy = (x + 1 async for x in aiter_list([4, 5]))
    collected = []
    async for value in lazy:
        collected.append(value)
    print(collected)

    # `async with` enters and always exits, including on the normal path.
    async with Resource("db") as name:
        print([name])

    # A truthy `__aexit__` suppresses the body exception.
    async with SuppressValueError():
        raise ValueError("suppressed")
    print("suppressed")

    # Built-in exception classes construct instances, and `type(e)` is the
    # same class object the exception name resolves to.
    error = ValueError("made")
    print(type(error) is ValueError)
    print(str(error))

    # TaskGroup joins overlapping tasks in creation order.
    print(await grouped())

    # `async for ... else` runs the else clause after normal exhaustion;
    # `continue` does not skip it.
    total = 0
    async for value in aiter_list([1, 2, 3]):
        if value == 2:
            continue
        total += value
    else:
        total += 100
    print(total)


asyncio.run(main())
