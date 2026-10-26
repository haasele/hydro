#!/usr/bin/env python3
"""Deterministic tests for the local benchmark threshold gate.

These cover the policy decisions behind P5-01c/Q-06: threshold checks stay a
local/release gate, `--check` is explicit, `--update` re-baselines with the
documented factor/floor, malformed or missing input fails loudly, and the CI
workflow keeps the report non-blocking while the harness self-tests block.

Run directly: `python3 scripts/test_check_benchmarks.py`
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).resolve().with_name("check_benchmarks.py")
ROOT = CHECK.parents[1]
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


def workflow_steps(path: Path) -> list[str]:
    """Splits a workflow file into step blocks (six-space list items)."""
    text = path.read_text(encoding="utf-8")
    return re.split(r"(?m)^      - ", text)[1:]


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
        "workloads": {"w": {"hydro-jit": {"baseline_ms": 8.0, "max_ms": max_ms}}},
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
        self.write(self.results_path, results(row("w", "hydro-jit", 10.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("1 passed, 0 failed", proc.stdout)

        explicit = self.run_check("--check")
        self.assertEqual(explicit.returncode, 0, explicit.stderr)

    def test_over_threshold_fails(self) -> None:
        self.write(self.results_path, results(row("w", "hydro-jit", 25.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1, proc.stdout)
        self.assertIn("FAIL", proc.stdout)
        self.assertIn("regression", proc.stderr)

    def test_check_flag_reports_the_same_failure(self) -> None:
        self.write(self.results_path, results(row("w", "hydro-jit", 25.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        default = self.run_check()
        explicit = self.run_check("--check")
        self.assertEqual(default.returncode, 1, default.stdout)
        self.assertEqual(explicit.returncode, 1, explicit.stdout)
        self.assertIn("1 failed", explicit.stdout)
        self.assertIn("FAIL", explicit.stdout)

    def test_missing_results_fails(self) -> None:
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check()
        self.assertEqual(proc.returncode, 1)
        self.assertIn("not found", proc.stderr)

    def test_unmatched_workloads_are_skipped(self) -> None:
        self.write(self.results_path, results(row("other", "hydro-jit", 10.0)))
        self.write(self.thresholds_path, thresholds(20.0))
        proc = self.run_check("--check", "-v")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("no comparable benchmarks", proc.stdout)

    def test_non_ok_rows_are_reported_as_skipped(self) -> None:
        self.write(
            self.results_path,
            results(
                row("w", "hydro-jit", 10.0),
                row("x", "hydro-jit", 1.0, status="error"),
            ),
        )
        self.write(
            self.thresholds_path,
            {
                "workloads": {
                    "w": {"hydro-jit": {"baseline_ms": 8.0, "max_ms": 20.0}},
                    "x": {"hydro-jit": {"baseline_ms": 1.0, "max_ms": 2.0}},
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
                row("w", "hydro-jit", 10.0),
                row("w", "hydro-aot", 20.0),
                row("w", "hydro-jit", 999.0, status="error"),
                row("ignored", "cpython", 1.0),
            ),
        )
        proc = self.run_check("--update", "--factor", "2.0", "-v")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(self.thresholds_path.read_text(encoding="utf-8"))
        jit = payload["workloads"]["w"]["hydro-jit"]
        aot = payload["workloads"]["w"]["hydro-aot"]
        self.assertEqual(jit["baseline_ms"], 10.0)
        self.assertEqual(jit["max_ms"], 20.0)
        self.assertEqual(aot["baseline_ms"], 20.0)
        self.assertEqual(aot["max_ms"], 40.0)
        self.assertNotIn("cpython", payload["workloads"].get("ignored", {}))

    def test_invalid_threshold_entry_fails(self) -> None:
        self.write(self.results_path, results(row("w", "hydro-jit", 10.0)))
        self.write(
            self.thresholds_path,
            {"workloads": {"w": {"hydro-jit": {"baseline_ms": 8.0, "max_ms": 0}}}},
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


class CiThresholdPolicyTests(unittest.TestCase):
    """The docs promise a non-blocking report and blocking harness self-tests."""

    def test_ci_workflow_exists(self) -> None:
        self.assertTrue(CI_WORKFLOW.is_file(), CI_WORKFLOW)

    def test_threshold_report_is_non_blocking_and_uses_check(self) -> None:
        steps = workflow_steps(CI_WORKFLOW)
        # Match the `run:` line only: comments in the preceding step also
        # mention the script name.
        report = [
            s
            for s in steps
            if re.search(r"(?m)^\s+run:.*scripts/check_benchmarks\.py", s)
        ]
        self.assertEqual(
            len(report), 1, "exactly one CI step may run check_benchmarks.py"
        )
        step = report[0]
        self.assertIn(
            "run: python3 scripts/check_benchmarks.py --check",
            step,
            "the report must use the explicit --check form",
        )
        self.assertRegex(
            step,
            r"(?m)^\s{8}continue-on-error: true\s*$",
            "the threshold report must not fail the build",
        )
        self.assertIn("non-blocking", step)

    def test_harness_self_tests_block(self) -> None:
        steps = workflow_steps(CI_WORKFLOW)
        self_tests = [
            s
            for s in steps
            if re.search(r"(?m)^\s+run:.*scripts/test_check_benchmarks\.py", s)
        ]
        self.assertEqual(len(self_tests), 1)
        step = self_tests[0]
        self.assertIn("run: python3 scripts/test_check_benchmarks.py", step)
        self.assertNotIn("continue-on-error", step)


if __name__ == "__main__":
    unittest.main()
