#!/usr/bin/env python3
"""Render weighted vote verdict to markdown report."""

from __future__ import annotations

import argparse
import json
import sys


SEVERITY_ORDER = {"critical": 0, "major": 1, "minor": 2}


def load_json(path: str):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def pick_severity(findings: list[dict]) -> str:
    values = [str(f.get("severity", "minor")).lower() for f in findings]
    if not values:
        return "minor"
    return sorted(values, key=lambda s: SEVERITY_ORDER.get(s, 99))[0]


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Render markdown report from verdict JSON"
    )
    parser.add_argument("input", help="Verdict JSON from weighted_vote.py")
    parser.add_argument(
        "-o", "--output", help="Markdown output path (defaults to stdout)"
    )
    args = parser.parse_args()

    data = load_json(args.input)
    accepted = data.get("accepted", [])

    accepted.sort(
        key=lambda item: (
            SEVERITY_ORDER.get(pick_severity(item.get("findings", [])), 99),
            -float(item.get("score", 0.0)),
        )
    )

    lines = []
    lines.append("## Bug Hunter Report")
    lines.append("")
    lines.append(f"- Threshold: `{data.get('threshold', 0.6)}`")
    lines.append(f"- Accepted findings: `{len(accepted)}`")
    lines.append("")
    lines.append("| 缺陷编号 | 位置 | 类型 | 严重级别 | 描述 | 建议修复 | 共识强度 |")
    lines.append("|---|---|---|---|---|---|---|")

    for item in accepted:
        findings = item.get("findings", [])
        first = findings[0] if findings else {}
        severity = pick_severity(findings)
        desc = str(first.get("description", "")).replace("\n", " ").strip()
        fix = (
            str(first.get("fix_code", "")).replace("\n", " ").strip()
            or "(需要补充修复建议)"
        )
        position = f"{item.get('file', '')}:{item.get('line', 0)}"
        lines.append(
            "| {id} | {pos} | {typ} | {sev} | {desc} | {fix} | {score}/10 |".format(
                id=item.get("bucket_id", "-"),
                pos=position,
                typ=item.get("primary_type", "unknown"),
                sev=severity,
                desc=desc,
                fix=fix,
                score=item.get("consensus_strength", 0.0),
            )
        )

    text = "\n".join(lines) + "\n"
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
