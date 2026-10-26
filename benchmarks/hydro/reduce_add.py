from functools import reduce
from operator import add

print(reduce(add, range(100000)))
