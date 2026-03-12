#!/usr/bin/env python3
"""Apply weighted consensus voting on bug buckets."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Dict


DEFAULT_WEIGHTS = {
    "Security Sentinel": 5.0,
    "Concurrency Engineer": 4.0,
    "Performance Analyst": 3.0,
    "Diverse Reviewer A": 2.0,
    "Diverse Reviewer B": 2.0,
    "Diverse Reviewer C": 2.0,
    "Diverse Reviewer D": 2.0,
    "Diverse Reviewer E": 2.0,
}


def load_json(path: str) -> Dict:
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def main() -> int:
    parser = argparse.ArgumentParser(description="Weighted vote for semantic buckets")
    parser.add_argument("input", help="Buckets JSON from semantic_bucket.py")
    parser.add_argument(
        "-o", "--output", help="Output verdict JSON (defaults to stdout)"
    )
    parser.add_argument(
        "--threshold", type=float, default=0.60, help="Accept threshold in [0,1]"
    )
    parser.add_argument("--weights", help="Optional JSON file for persona weights")
    args = parser.parse_args()

    data = load_json(args.input)
    buckets = data.get("buckets", [])
    weights = DEFAULT_WEIGHTS.copy()
    if args.weights:
        weights.update(load_json(args.weights))

    accepted = []
    rejected = []

    for bucket in buckets:
        num = 0.0
        den = 0.0
        for finding in bucket.get("findings", []):
            agent = str(finding.get("agent", "Diverse Reviewer A"))
            conf = float(finding.get("confidence", 0.5))
            weight = float(weights.get(agent, 1.0))
            penalty = 0.9 if not str(finding.get("fix_code", "")).strip() else 1.0
            num += weight * conf * penalty
            den += weight

        score = (num / den) if den else 0.0
        verdict = {
            "bucket_id": bucket.get("bucket_id"),
            "file": bucket.get("file"),
            "line": bucket.get("line"),
            "primary_type": bucket.get("primary_type"),
            "type_conflict": bucket.get("type_conflict", False),
            "evidence_count": bucket.get("evidence_count", 0),
            "score": round(score, 4),
            "consensus_strength": round(score * 10, 2),
            "findings": bucket.get("findings", []),
        }

        if score >= args.threshold:
            accepted.append(verdict)
        else:
            rejected.append(verdict)

    payload = {
        "threshold": args.threshold,
        "accepted": sorted(accepted, key=lambda x: x["score"], reverse=True),
        "rejected": sorted(rejected, key=lambda x: x["score"], reverse=True),
    }
    text = json.dumps(payload, ensure_ascii=False, indent=2)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
