#!/usr/bin/env python3
"""Select contentious buckets for adversarial debate."""

from __future__ import annotations

import argparse
import json
import sys


def load_json(path: str):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def bucket_score(bucket: dict) -> float:
    findings = bucket.get("findings", [])
    if not findings:
        return 0.0
    return sum(float(f.get("confidence", 0.5)) for f in findings) / len(findings)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Pick debate candidates from bucket list"
    )
    parser.add_argument("input", help="Buckets JSON from semantic_bucket.py")
    parser.add_argument("-o", "--output", help="Output debate candidates JSON")
    parser.add_argument(
        "--low", type=float, default=0.50, help="Debate lower score bound"
    )
    parser.add_argument(
        "--high", type=float, default=0.60, help="Debate upper score bound"
    )
    args = parser.parse_args()

    data = load_json(args.input)
    buckets = data.get("buckets", [])
    candidates = []

    for b in buckets:
        score = bucket_score(b)
        conflict = bool(b.get("type_conflict", False))
        if conflict or (args.low <= score < args.high):
            candidates.append(
                {
                    "bucket_id": b.get("bucket_id"),
                    "file": b.get("file"),
                    "line": b.get("line"),
                    "score": round(score, 4),
                    "type_conflict": conflict,
                    "reason": "type_conflict" if conflict else "borderline_score",
                    "findings": b.get("findings", []),
                }
            )

    payload = {"candidates": candidates}
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
