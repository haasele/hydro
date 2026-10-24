nums = [10, 20, 30]
print(nums)
print(nums[0], nums[-1], len(nums))

nums[1] = 99
print(nums)
print(nums == [10, 99, 30], nums + [40])

total = 0
for n in nums:
    total += n
print(total)

nested = [[1, 2], [3]]
print(nested[1][0])

scores = {"a": 1, "b": 2}
scores["c"] = 3
scores["a"] = 10
print(scores)
print(len(scores), scores["b"])

for key in scores:
    print(key)

try:
    print(scores["missing"])
except KeyError as e:
    print("missing", e)

chars = ""
for ch in "abc":
    chars += ch
print(chars)
