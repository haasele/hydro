"""Deterministic NumPy 2.5.0 smoke case for the ecosystem gate.

The program only prints values formatted explicitly (``int``/``float``/``list``)
so a stdout diff means an actual behavioural difference, not a different repr.
No timestamps, host paths or unseeded randomness reach stdout.
"""

import numpy as np

print("numpy", np.__version__)

a = np.arange(12, dtype=np.int64).reshape(3, 4)
print("array", a.shape, a.dtype.str, a.tolist())
print("reduce", int(a.sum()), round(float(a.mean()), 6), int(a.max()))
print("axis", a.sum(axis=1).tolist())
print("slice", a[1:, ::2].tolist())
print("transpose", a.T.shape, a.T[0].tolist())

b = np.linspace(0.0, 1.0, 5)
print("linspace", np.round(b, 6).tolist())
print("broadcast", (a[:2] + np.array([10, 20, 30, 40])).tolist())
print("ufunc", np.round(np.sqrt(np.arange(1, 6, dtype=np.float64)), 6).tolist())
print("where", np.where(a % 2 == 0, a, -1).tolist())
print("clip", np.clip(a, 2, 7).tolist())
print("unique", np.unique(a % 5).tolist())

matrix = np.array([[4.0, 1.0], [1.0, 3.0]])
print("det", round(float(np.linalg.det(matrix)), 6))
print("inv", np.round(np.linalg.inv(matrix), 6).tolist())
print("solve", np.round(np.linalg.solve(matrix, np.array([1.0, 2.0])), 6).tolist())
eigenvalues, _ = np.linalg.eigh(matrix)
print("eigh", np.round(eigenvalues, 6).tolist())

left = np.array([[1, 2, 3], [4, 5, 6]], dtype=np.int32)
right = np.array([[7, 8], [9, 10], [11, 12]], dtype=np.int32)
print("matmul", (left @ right).tolist())
print("einsum", np.einsum("ij,jk->ik", left, right).tolist())

rng = np.random.default_rng(20250101)
print("random", np.round(rng.random(4), 6).tolist())
print("random_int", rng.integers(0, 100, size=5, endpoint=False).tolist())

np.save("array.npy", a)
loaded = np.load("array.npy")
print("save_load", loaded.shape, loaded.dtype.str, np.array_equal(a, loaded))

print("argmax", np.argmax(a, axis=1).tolist())
print("sort", np.sort(np.array([3, 1, 2], dtype=np.int64)).tolist())
print("astype", round(float(a.astype(np.float64).sum()), 2))

stamp = np.datetime64("2020-01-01") + np.timedelta64(5, "D")
print("datetime64", str(stamp), np.dtype("float64").itemsize)
