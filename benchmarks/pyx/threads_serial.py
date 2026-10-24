total = 0
block = 0
while block < 4:
    i = 0
    while i < 200000:
        total += i
        i += 1
    block += 1
print(total)
