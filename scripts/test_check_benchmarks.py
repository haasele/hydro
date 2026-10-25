#!/usr/bin/env python3
"""Deterministic tests for the local benchmark threshold gate.

These cover the policy decisions behind P5-01c/Q-06: threshold checks stay a
local/release gate, `--check` is explicit, `--update` re-baselines with the
documented factor/floor, and malformed or missing input fails loudly.

Run directly: `python3 scripts/test_check_benchmarks.py`
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().with_name("check_benchmarks.py")


def results(*rows: dict) -> dict:
    return {"meta": {"timestamp": "test"}, "results": list(rows)}


def row(workload: str, impl: str, median: float, status: str = "ok") -> dict:
    return {
        "workload": workload,
        "impl": impl,
        "status": status,
        "median_ms": median,
    }


def thresholds(max_ms: float) -> dict:
    return {
        "schema": 1,
        "workloads": {"w": {"pyx-jit": {"baseline_ms": 8.0, "max_ms": max_ms}}},
    }


class CheckBenchmarksTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.results_path = self.tmp / "results.json"
        self.thresholds_path = self.tmp / "thresholds.json"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, path: Path, payload: dict) -> None:
        path.write_text(json.dumps(payload), encoding="utf-8")

    def run_check(self, *extra: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(CHECK),
                "--results",
                str(self.results_path),
                "--thresholds",
                str(self.thresholds_path),
                *extra,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def test_within_threshold_passes(self) -> None:
        self.write(self.results_path, results(row("w", "pyx-jit", 10.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("1 passed, 0 failed", proc.stdout)

        explicit = self.run_check("--check")
        self.assertEqual(explicit.returncode, 0, explicit.stderr)

    def test_over_threshold_fails(self) -> None:
        self.write(self.results_path, results(row("w", "pyx-jit", 25.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1, proc.stdout)
        self.assertIn("FAIL", proc.stdout)
        self.assertIn("regression", proc.stderr)

    def test_missing_results_fails(self) -> None:
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("not found", proc.stderr)

    def test_unmatched_workloads_are_skipped(self) -> None:
        self.write(self.results_path, results(row("other", "pyx-jit", 10.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check("--check", "-v")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("no comparable benchmarks", proc.stdout)

    def test_non_ok_rows_are_reported_as_skipped(self) -> None:
        self.write(
            self.results_path,
            results(
                row("w", "pyx-jit", 10.0),
                row("x", "pyx-jit", 1.0, status="error"),
            ),
        )
        self.write(
            self.thresholds_path,
            {
                "workloads": {
                    "w": {"pyx-jit": {"baseline_ms": 8.0, "max_ms": 20.0}},
                    "x": {"pyx-jit": {"baseline_ms": 1.0, "max_ms": 2.0}},
                }
            },
        )
        proc = self.run_check("-v")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("1 passed, 0 failed, 1 skipped", proc.stdout)

    def test_check_and_update_are_mutually_exclusive(self) -> None:
        proc = self.run_check("--check", "--update")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("not allowed with argument", proc.stderr)

    def test_update_uses_factor_and_floor(self) -> None:
        self.write(
            self.results_path,
            results(
                row("w", "pyx-jit", 10.0),
                row("w", "pyx-aot", 20.0),
                row("w", "pyx-jit", 999.0, status="error"),
                row("ignored", "cpython", 1.0),
            ),
        )
        proc = self.run_check("--update", "--factor", "2.0", "-v")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(self.thresholds_path.read_text(encoding="utf-8"))
        jit = payload["workloads"]["w"]["pyx-jit"]
        aot = payload["workloads"]["w"]["pyx-aot"]
        self.assertEqual(jit["baseline_ms"], 10.0)
        self.assertEqual(jit["max_ms"], 20.0)
        self.assertEqual(aot["baseline_ms"], 20.0)
        self.assertEqual(aot["max_ms"], 40.0)
        self.assertNotIn("cpython", payload["workloads"].get("ignored", {}))

    def test_invalid_threshold_entry_fails(self) -> None:
        self.write(self.results_path, results(row("w", "pyx-jit", 10.0)))
        self.write(
            self.thresholds_path,
            {"workloads": {"w": {"pyx-jit": {"baseline_ms": 8.0, "max_ms": 0}}}},
        )
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("positive 'max_ms'", proc.stderr)

    def test_malformed_results_fails(self) -> None:
        self.results_path.write_text("[]", encoding="utf-8")
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("expected an object", proc.stderr)


if __name__ == "__main__":
    unittest.main()
