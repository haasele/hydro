n = 100000
out = []
i = 0
while i < n:
    out.append(i * 2)
    i += 1
print(len(out), out[0] + out[-1])
