#!/usr/bin/env python3
"""Compare obstacle-course correctness and report latency changes.

Two correctness rules gate the run:
  - A stage that passes on base and fails on the candidate is a regression
    and fails the job.
  - A stage that fails on BOTH base and candidate must be listed in the
    expected-failures file or the job fails. Without this rule a stage broken
    before the PR stays invisible forever: observer-intersection was silently
    failing on every run for a month because only regressions were checked.

The expected-failures file (scripts/ci/expected_obstacle_failures.txt) is
read from the trusted base revision by ci.yml, like this script itself, so a
PR cannot acknowledge its own breakage; an entry takes effect only after the
change adding it is merged.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import statistics
from pathlib import Path


MAX_RESULT_BYTES = 10 * 1024 * 1024
EXPECTED_STAGE_COUNT = 33


def load_result(path: Path) -> dict:
    if path.stat().st_size > MAX_RESULT_BYTES:
        raise ValueError(f"{path} is unexpectedly large")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("results"), list):
        raise ValueError(f"{path} is not an obstacle-course result")
    return value


def result_map(value: dict) -> dict[str, dict]:
    mapped: dict[str, dict] = {}
    for result in value["results"]:
        if not isinstance(result, dict) or not isinstance(result.get("name"), str):
            raise ValueError("obstacle result has an invalid stage")
        name = result["name"]
        if name in mapped:
            raise ValueError(f"duplicate obstacle stage: {name}")
        median_ms = result.get("median_ms")
        if (
            not isinstance(result.get("pass"), bool)
            or isinstance(median_ms, bool)
            or not isinstance(median_ms, (int, float))
            or not math.isfinite(median_ms)
            or median_ms < 0
        ):
            raise ValueError(f"invalid obstacle result for {name}")
        mapped[name] = result
    return mapped


def add_summary(markdown: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as output:
            output.write(markdown)
    else:
        print(markdown)


def load_expected_failures(path: Path | None) -> set[str]:
    """Stage names acknowledged as failing, one per line, # comments."""
    if path is None or not path.exists():
        return set()
    entries: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        name = line.split("#", 1)[0].strip()
        if name:
            entries.add(name)
    return entries


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--expected-failures", type=Path, default=None)
    args = parser.parse_args()

    base = result_map(load_result(args.base))
    candidate = result_map(load_result(args.candidate))
    if len(base) != EXPECTED_STAGE_COUNT:
        raise SystemExit(
            f"base obstacle result has {len(base)} stages; expected {EXPECTED_STAGE_COUNT}"
        )
    if len(candidate) != EXPECTED_STAGE_COUNT:
        raise SystemExit(
            f"candidate obstacle result has {len(candidate)} stages; expected {EXPECTED_STAGE_COUNT}"
        )
    if set(base) != set(candidate):
        missing = sorted(set(base) - set(candidate))
        extra = sorted(set(candidate) - set(base))
        raise SystemExit(f"obstacle stage mismatch; missing={missing}, extra={extra}")

    regressions = sorted(name for name in base if base[name]["pass"] and not candidate[name]["pass"])
    improvements = sorted(name for name in base if not base[name]["pass"] and candidate[name]["pass"])

    expected = load_expected_failures(args.expected_failures)
    persistent = sorted(
        name for name in base
        if not base[name]["pass"] and not candidate[name]["pass"]
    )
    unacknowledged = sorted(name for name in persistent if name not in expected)
    unknown_entries = sorted(name for name in expected if name not in base)
    recovered_entries = sorted(
        name for name in expected if name in candidate and candidate[name]["pass"]
    )
    for name in persistent:
        state = "acknowledged" if name in expected else "UNACKNOWLEDGED"
        print(f"::warning::obstacle stage {name} fails on base and candidate ({state})")
    for name in unknown_entries:
        print(f"::warning::expected-failures entry {name} matches no obstacle stage")
    for name in recovered_entries:
        print(f"::warning::expected-failures entry {name} passes now; remove it")
    ratios = []
    slow = []
    for name in sorted(base):
        base_ms = float(base[name]["median_ms"])
        candidate_ms = float(candidate[name]["median_ms"])
        if base_ms > 0:
            ratio = candidate_ms / base_ms
            ratios.append(ratio)
            if ratio > 1.20 and candidate_ms - base_ms > 10:
                slow.append((name, base_ms, candidate_ms, ratio))

    base_passed = sum(1 for result in base.values() if result["pass"])
    candidate_passed = sum(1 for result in candidate.values() if result["pass"])
    median_ratio = statistics.median(ratios) if ratios else 1.0
    lines = [
        "## Obstacle comparison\n",
        "| Metric | Base | Candidate |\n",
        "| --- | ---: | ---: |\n",
        f"| Correct stages | {base_passed}/{len(base)} | {candidate_passed}/{len(candidate)} |\n",
        f"| Median per-stage latency ratio | 1.00x | {median_ratio:.2f}x |\n",
    ]
    if improvements:
        lines.append(f"\nImproved stages: {', '.join(improvements)}\n")
    if regressions:
        lines.append(f"\nRegressed stages: {', '.join(regressions)}\n")
    if persistent:
        lines.append(
            "\nStages failing on both base and candidate: "
            + ", ".join(
                f"`{name}`" + ("" if name in expected else " **(unacknowledged)**")
                for name in persistent
            )
            + "\n"
        )
    if recovered_entries:
        lines.append(
            "\nStale expected-failures entries (now passing): "
            + ", ".join(recovered_entries)
            + "\n"
        )
    if slow:
        lines.append("\nStages above the reporting threshold:\n\n")
        for name, base_ms, candidate_ms, ratio in slow:
            lines.append(f"- `{name}`: {base_ms:.1f} ms to {candidate_ms:.1f} ms ({ratio:.2f}x)\n")
            print(f"::warning::obstacle latency {name}: {ratio:.2f}x base")
    add_summary("".join(lines))

    if regressions:
        raise SystemExit("candidate introduces new obstacle-course failures")
    if unacknowledged:
        raise SystemExit(
            "obstacle stages fail on base and candidate without an "
            "expected-failures entry: " + ", ".join(unacknowledged)
        )


if __name__ == "__main__":
    main()
