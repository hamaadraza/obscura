#!/usr/bin/env python3
"""Render a corpus of real sites repeatedly and record a per-site distribution.

A single pass over real sites cannot answer whether a change helped or hurt.
Measured on a 106-site corpus, a quarter of the sites moved by more than 200
characters between two runs of the *same* build, and the set of outright
failures was almost completely different each time. Summary counts computed
from one pass ("72 rendered" vs "68 rendered") therefore say nothing, and this
has already caused a real mistake: a sweep was read as a regression when the
difference was entirely run-to-run noise.

So each site is rendered several times and reported as a distribution rather
than a number. The spread that comes out is the site's own noise, which is what
`compare_sweep.py` uses to decide whether a later change moved it. Sites are
keyed by host, so a corpus can be reordered or extended without invalidating an
older aggregate.

Usage:
    python scripts/ci/sweep_sites.py --bin ./target/release/obscura \\
        --sites corpus.txt --runs 3 --out base.json

The corpus is a newline-separated list of URLs; blank lines and lines starting
with `#` are skipped. The obscura-benchmark repo carries one under
`realworld/sites.txt`.
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

# Returned by the page for every site. Kept deliberately small: anything that
# varies for reasons unrelated to rendering (timings, ids, nonces) would show
# up as noise and raise every site's threshold.
PAGE_PROBE = """(function () {
  function text() {
    try {
      var t = document.body ? (document.body.innerText || '') : '';
      return t.replace(/\\s+/g, ' ').trim();
    } catch (e) { return ''; }
  }
  var body = text();
  var challenge = /just a moment|checking your browser|enable javascript|verify you are human|access denied|are you a robot|unusual traffic|captcha|attention required|security check/i;
  return JSON.stringify({
    title: (document.title || '').replace(/\\s+/g, ' ').trim().slice(0, 80),
    textLen: body.length,
    elements: document.querySelectorAll('*').length,
    blocked: challenge.test(body.slice(0, 600)) || challenge.test(document.title || ''),
  });
})()"""

MAX_OUTPUT_BYTES = 64 * 1024 * 1024


def read_corpus(path: Path) -> list[str]:
    urls = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            urls.append(line)
    if not urls:
        raise ValueError(f"{path} contains no URLs")
    return urls


def host_of(url: str) -> str:
    return url.split("//", 1)[-1].split("/", 1)[0].lower()


def run_once(binary: str, urls: list[str], concurrency: int, timeout: int,
             stealth: bool) -> dict[str, dict]:
    """One pass over the corpus. Returns per-host observations."""
    command = [binary]
    if stealth:
        command.append("--stealth")
    command += ["scrape", *urls, "--eval", PAGE_PROBE,
                "--concurrency", str(concurrency), "--timeout", str(timeout)]
    completed = subprocess.run(
        command, capture_output=True, text=True,
        timeout=timeout * len(urls) + 600,
    )
    if len(completed.stdout) > MAX_OUTPUT_BYTES:
        raise ValueError("scrape produced an unexpectedly large result")
    start = completed.stdout.find("{")
    if start < 0:
        raise ValueError(f"no result from scrape: {completed.stderr[-400:]}")
    payload = json.loads(completed.stdout[start:])

    observations: dict[str, dict] = {}
    for entry in payload.get("results", []):
        host = host_of(entry.get("url", "?"))
        error = entry.get("error")
        if error:
            observations[host] = {"status": "failed", "textLen": 0, "detail": str(error)[:120]}
            continue
        raw = entry.get("eval")
        if not raw:
            # An eval that produced nothing is not the same as a page that
            # rendered nothing; `eval_error` says which.
            detail = entry.get("eval_error") or "no eval result"
            observations[host] = {"status": "failed", "textLen": 0, "detail": str(detail)[:120]}
            continue
        try:
            page = json.loads(raw)
        except (TypeError, ValueError):
            observations[host] = {"status": "failed", "textLen": 0, "detail": "unparsable probe"}
            continue
        length = int(page.get("textLen", 0))
        if page.get("blocked"):
            status = "blocked"
        elif length < 200:
            status = "thin"
        else:
            status = "rendered"
        observations[host] = {
            "status": status,
            "textLen": length,
            "elements": int(page.get("elements", 0)),
            "title": page.get("title", ""),
        }
    return observations


def aggregate(passes: list[dict[str, dict]]) -> dict:
    hosts = sorted({host for observations in passes for host in observations})
    sites: dict[str, dict] = {}
    for host in hosts:
        seen = [observations[host] for observations in passes if host in observations]
        lengths = [item["textLen"] for item in seen]
        # Element count is structural, so it survives a change in what a text
        # metric *means*. Comparing this corpus across a build that redefined
        # `innerText` reported 58 sites "rendering less" when the change was a
        # correctness fix -- the old build returned script source as rendered
        # text. A second, semantics-independent metric makes that visible.
        elements = [item.get("elements", 0) for item in seen]
        statuses = [item["status"] for item in seen]
        # The majority status, so one flaky failure does not reclassify a site
        # that renders on every other pass.
        ranked = sorted(set(statuses), key=lambda s: (-statuses.count(s), s))
        sites[host] = {
            "status": ranked[0],
            "statuses": statuses,
            "median": statistics.median(lengths) if lengths else 0,
            "min": min(lengths) if lengths else 0,
            "max": max(lengths) if lengths else 0,
            "elements": statistics.median(elements) if elements else 0,
            "samples": len(seen),
            "title": next((item.get("title", "") for item in seen if item.get("title")), ""),
        }
    return {"runs": len(passes), "sites": sites}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, help="path to the obscura binary")
    parser.add_argument("--sites", required=True, type=Path, help="newline-separated URLs")
    parser.add_argument("--runs", type=int, default=3,
                        help="passes over the corpus (default 3; more narrows the noise floor)")
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--timeout", type=int, default=45, help="per-site seconds")
    parser.add_argument("--stealth", action="store_true")
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()

    if args.runs < 2:
        parser.error("at least 2 runs are needed to observe any spread at all")

    urls = read_corpus(args.sites)
    passes = []
    for index in range(args.runs):
        print(f"pass {index + 1}/{args.runs} over {len(urls)} sites...", file=sys.stderr)
        passes.append(run_once(args.bin, urls, args.concurrency, args.timeout, args.stealth))

    result = aggregate(passes)
    args.out.write_text(json.dumps(result, indent=1), encoding="utf-8")

    counts: dict[str, int] = {}
    for site in result["sites"].values():
        counts[site["status"]] = counts.get(site["status"], 0) + 1
    unstable = sum(1 for site in result["sites"].values() if len(set(site["statuses"])) > 1)
    summary = "  ".join(f"{name}: {count}" for name, count in sorted(counts.items()))
    print(f"{len(result['sites'])} sites over {args.runs} runs -> {args.out}")
    print(f"  {summary}")
    print(f"  {unstable} sites did not report the same status every run")
    return 0


if __name__ == "__main__":
    sys.exit(main())
