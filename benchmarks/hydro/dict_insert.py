d = {}
i = 0
while i < 5000:
    d[i] = i * i
    i += 1
print(len(d), d[4999])
