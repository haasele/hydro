import asyncio


def gen(n):
    i = 0
    while i < n:
        yield i
        i += 1


print(list(gen(4)))


async def work():
    return 7


async def main():
    return (await work()) + 1


print(asyncio.run(main()))


async def values():
    yield 1
    yield 2


async def collect():
    out = []
    async for value in values():
        out.append(value)
    return out


print(asyncio.run(collect()))
