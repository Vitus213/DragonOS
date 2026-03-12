#!/usr/bin/env python3
"""Run Bug Hunter Stage1/3/4 pipeline with filesystem artifacts."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent


def run(cmd: list[str]) -> None:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(proc.returncode)


def ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description="Run bug-hunter pipeline stages")
    parser.add_argument("--diff-file", help="Optional unified diff file for Stage1")
    parser.add_argument(
        "--raw-findings", required=True, help="Stage2 raw findings JSON file"
    )
    parser.add_argument(
        "--out-dir", default="artifacts", help="Artifact output directory"
    )
    parser.add_argument(
        "--passes", type=int, default=8, help="Shuffle pass count for Stage1"
    )
    parser.add_argument(
        "--threshold", type=float, default=0.6, help="Consensus threshold"
    )
    args = parser.parse_args()

    out_dir = Path(args.out_dir)
    ensure_dir(out_dir)

    stage_files = {
        "redacted_diff": out_dir / "redacted.diff",
        "shuffled": out_dir / "shuffled_passes.json",
        "buckets": out_dir / "buckets.json",
        "debate": out_dir / "debate_candidates.json",
        "verdict": out_dir / "verdict.json",
        "report": out_dir / "bug_hunter_report.md",
    }

    if args.diff_file:
        run(
            [
                sys.executable,
                str(ROOT / "redact_sensitive.py"),
                args.diff_file,
                "-o",
                str(stage_files["redacted_diff"]),
            ]
        )
        run(
            [
                sys.executable,
                str(ROOT / "shuffle_diff.py"),
                str(stage_files["redacted_diff"]),
                "--passes",
                str(args.passes),
                "-o",
                str(stage_files["shuffled"]),
            ]
        )

    if not os.path.exists(args.raw_findings):
        raise SystemExit(f"raw findings not found: {args.raw_findings}")

    run(
        [
            sys.executable,
            str(ROOT / "semantic_bucket.py"),
            args.raw_findings,
            "-o",
            str(stage_files["buckets"]),
        ]
    )
    run(
        [
            sys.executable,
            str(ROOT / "debate_picker.py"),
            str(stage_files["buckets"]),
            "-o",
            str(stage_files["debate"]),
        ]
    )
    run(
        [
            sys.executable,
            str(ROOT / "weighted_vote.py"),
            str(stage_files["buckets"]),
            "--threshold",
            str(args.threshold),
            "-o",
            str(stage_files["verdict"]),
        ]
    )
    run(
        [
            sys.executable,
            str(ROOT / "render_report.py"),
            str(stage_files["verdict"]),
            "-o",
            str(stage_files["report"]),
        ]
    )

    summary = {
        "out_dir": str(out_dir),
        "artifacts": {k: str(v) for k, v in stage_files.items() if v.exists()},
    }
    sys.stdout.write(json.dumps(summary, ensure_ascii=False) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
