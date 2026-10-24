import argparse
import collections
import re


parser = argparse.ArgumentParser(prog="demo")
parser.add_argument("--n", type=int, default=1)
parser.add_argument("--verbose", action="store_true")
parser.add_argument("input")
args = parser.parse_args(["--n", "5", "--verbose", "file.txt"])
print(args.n, args.verbose, args.input)

counts = collections.defaultdict(int)
counts["x"] += 1
print(counts["x"])

groups = collections.defaultdict(list)
groups["a"].append(1)
print(groups)

print(re.findall("[0-9]+", "a1b22"))
print(re.sub("a+", "x", "baa"))
