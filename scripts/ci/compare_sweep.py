#!/usr/bin/env python3
"""Compare two site-sweep aggregates and report only what moved beyond noise.

Real sites are not reproducible: they serve different content, run experiments,
and sometimes fail for their own reasons. Comparing two single passes therefore
produces a long list of differences that mean nothing, which is worse than no
comparison at all because it looks like evidence.

Each site here carries the spread observed across the runs that produced it, so
a change counts only when it exceeds what that site was already doing on its
own. A site that swings by 5,000 characters between runs of one build has to
move a great deal more than one that is stable to the byte.

Sites present in only one aggregate are reported separately rather than being
treated as changes, so extending the corpus does not read as a regression.

Usage:
    python scripts/ci/compare_sweep.py base.json candidate.json
    python scripts/ci/compare_sweep.py base.json candidate.json --fail-on-regression
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

MAX_RESULT_BYTES = 32 * 1024 * 1024
# A floor under the per-site spread. Two runs can agree exactly by luck, and a
# site whose observed spread is zero must not therefore treat a single
# character as significant.
MINIMUM_NOISE = 200
# Rendering statuses ordered worst to best, so a status move has a direction.
STATUS_RANK = {"failed": 0, "blocked": 1, "thin": 2, "rendered": 3}


def load(path: Path) -> dict:
    if path.stat().st_size > MAX_RESULT_BYTES:
        raise ValueError(f"{path} is unexpectedly large")
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("sites"), dict):
        raise ValueError(f"{path} is not a sweep aggregate")
    return value


def spread(site: dict) -> float:
    """How much this site moved on its own, across the runs behind it."""
    return max(0.0, float(site.get("max", 0)) - float(site.get("min", 0)))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--fail-on-regression", action="store_true",
                        help="exit non-zero when a site renders less or drops status")
    args = parser.parse_args()

    base = load(args.base)
    candidate = load(args.candidate)
    base_sites = base["sites"]
    candidate_sites = candidate["sites"]

    shared = sorted(set(base_sites) & set(candidate_sites))
    only_base = sorted(set(base_sites) - set(candidate_sites))
    only_candidate = sorted(set(candidate_sites) - set(base_sites))

    improved: list[tuple[str, float, float, float]] = []
    regressed: list[tuple[str, float, float, float]] = []
    status_moves: list[tuple[str, str, str]] = []
    unstable = 0

    for host in shared:
        before, after = base_sites[host], candidate_sites[host]
        # The noise a change has to clear is what both aggregates already
        # showed on their own.
        threshold = max(MINIMUM_NOISE, spread(before) + spread(after))
        delta = float(after.get("median", 0)) - float(before.get("median", 0))
        if abs(delta) > threshold:
            (improved if delta > 0 else regressed).append(
                (host, float(before.get("median", 0)), float(after.get("median", 0)), threshold)
            )
        if before.get("status") != after.get("status"):
            status_moves.append((host, before.get("status", "?"), after.get("status", "?")))
        if len(set(after.get("statuses", []))) > 1:
            unstable += 1

    runs = f"{base.get('runs', '?')} vs {candidate.get('runs', '?')} runs"
    print(f"{len(shared)} sites compared ({runs})")
    print(f"  {unstable} unstable in the candidate (status varied between its own runs)")

    def report(title: str, rows: list[tuple[str, float, float, float]]) -> None:
        print(f"\n--- {title}: {len(rows)} ---")
        for host, before, after, threshold in sorted(rows, key=lambda r: abs(r[2] - r[1]), reverse=True):
            print(f"  {host:32} {before:>9.0f} -> {after:>9.0f}   (noise floor {threshold:.0f})")

    report("rendered less", regressed)
    report("rendered more", improved)

    if status_moves:
        print(f"\n--- status changed: {len(status_moves)} ---")
        for host, before, after in status_moves:
            direction = "worse" if STATUS_RANK.get(after, 9) < STATUS_RANK.get(before, 9) else "better"
            print(f"  {host:32} {before} -> {after}  ({direction})")

    if only_base or only_candidate:
        print(f"\n--- corpus differs: {len(only_base)} only in base, "
              f"{len(only_candidate)} only in candidate ---")
        for host in (only_base + only_candidate)[:10]:
            print(f"  {host}")

    # A text measure only means the same thing on both sides if neither build
    # changed what it measures. When the two disagree wholesale on text while
    # agreeing on structure, suspect the metric before the rendering: comparing
    # across a build that redefined `innerText` reported 58 sites "rendering
    # less" when the change was a correctness fix, the older build having
    # returned script source as rendered text.
    moved = {row[0] for row in regressed} | {row[0] for row in improved}
    same_structure = [
        host for host in moved
        if base_sites[host].get("elements") and candidate_sites[host].get("elements")
        and abs(float(candidate_sites[host]["elements"]) - float(base_sites[host]["elements"]))
        <= 0.05 * float(base_sites[host]["elements"])
    ]
    if len(same_structure) > max(5, len(moved) // 2):
        print(f"\nNOTE: {len(same_structure)} of {len(moved)} moved sites kept their "
              f"element count within 5%. That pattern usually means the two builds "
              f"disagree about what the text measure counts, not that pages rendered "
              f"differently. Check one site by hand before reading these as regressions.")

    if not regressed and not improved and not status_moves:
        print("\nNothing moved beyond each site's own run-to-run noise.")

    dropped_status = [
        move for move in status_moves
        if STATUS_RANK.get(move[2], 9) < STATUS_RANK.get(move[1], 9)
    ]
    if args.fail_on_regression and (regressed or dropped_status):
        print(f"\nFAIL: {len(regressed)} sites rendered less, "
              f"{len(dropped_status)} dropped status")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
