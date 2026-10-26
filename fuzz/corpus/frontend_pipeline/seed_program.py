# Seed: a program that exercises most frontend stages.
def add(a: int, b: int) -> int:
    return a + b


def describe(value: str) -> str:
    match value:
        case "" | None:
            return "empty"
        case _:
            return f"{value!r}: {len(value)}"


items = [add(i, i * 2) for i in range(6)]
total = sum(items)
print(describe("tea"), total)
