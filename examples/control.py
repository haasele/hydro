total = 0
for i in range(0, 10, 2):
    if i == 6:
        continue
    total += i
print(total)

n = 0
while n < 100:
    n += 1
    if n == 3:
        break
print(n)

x = 7
if x < 0:
    print("negative")
elif x == 0:
    print("zero")
elif x < 5:
    print("small")
else:
    print("large")

for i in range(3, 0, -1):
    print(i)
