"""Deterministic pandas 3.1.0 smoke case for the ecosystem gate.

Covers Series/DataFrame construction, reductions, groupby, merge, concat,
missing values, CSV round-trip, datetime accessors and pivot tables. All output
is explicitly converted to Python lists/scalars or sorted mappings.
"""

import pandas as pd

print("pandas", pd.__version__)

series = pd.Series([4, 1, 3, 2], index=["d", "a", "c", "b"], dtype="int64")
print("series", series.tolist(), series.index.tolist(), str(series.dtype))
print(
    "stats",
    int(series.sum()),
    round(float(series.mean()), 6),
    int(series.max()),
    series.idxmax(),
)
print("sorted", series.sort_values().tolist())

frame = pd.DataFrame(
    {
        "group": ["x", "x", "y", "y"],
        "value": [1, 3, 2, 8],
        "flag": [True, False, True, False],
    }
)
print("shape", frame.shape)
print("columns", frame.columns.tolist())
print("dtypes", [str(dtype) for dtype in frame.dtypes.tolist()])
print("value_sum", int(frame["value"].sum()), round(float(frame["value"].mean()), 6))
print("groupby_sum", frame.groupby("group", sort=True)["value"].sum().to_dict())
print(
    "groupby_agg",
    frame.groupby("group", sort=True)["value"].agg(["count", "min", "max"]).to_dict(
        orient="list"
    ),
)

left = pd.DataFrame({"key": ["a", "b", "c"], "value": [1, 2, 3]})
right = pd.DataFrame({"key": ["b", "c", "d"], "extra": [20, 30, 40]})
merged = (
    left.merge(right, on="key", how="left")
    .sort_values("key")
    .reset_index(drop=True)
)
print(
    "merge",
    merged["key"].tolist(),
    merged["extra"].fillna(-1).astype("int64").tolist(),
)

concatenated = pd.concat([frame, frame], ignore_index=True)
print("concat", concatenated.shape, int(concatenated["value"].sum()))

missing = pd.Series([1.0, None, 3.0])
print("missing", missing.isna().tolist(), missing.fillna(0.0).tolist())
print("ffill", missing.ffill().tolist())

csv_frame = pd.DataFrame({"a": [1, 2], "b": ["x", "y"]})
csv_frame.to_csv("frame.csv", index=False)
loaded = pd.read_csv("frame.csv")
print("csv", loaded["a"].tolist(), loaded["b"].tolist())

dates = pd.Series(pd.to_datetime(["2021-01-01", "2021-01-04", "2021-02-01"]))
print("datetime", dates.dt.strftime("%Y-%m-%d").tolist())
print("delta_days", int((dates.iloc[1] - dates.iloc[0]).days))
print("dayofweek", dates.dt.dayofweek.tolist(), "month", dates.dt.month.tolist())

pivot = frame.pivot_table(index="group", values="value", aggfunc="sum")
print("pivot", pivot.index.tolist(), pivot["value"].tolist())

print("cast", frame.astype({"value": "float64"})["value"].sum())
