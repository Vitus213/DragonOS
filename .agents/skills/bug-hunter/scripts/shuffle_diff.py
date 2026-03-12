#!/usr/bin/env python3
"""Generate multiple shuffled passes from a unified diff."""

from __future__ import annotations

import argparse
import json
import random
import re
import sys
from dataclasses import dataclass
from typing import List


DIFF_START = re.compile(r"^diff --git ")
HUNK_START = re.compile(r"^@@ ")


@dataclass
class Hunk:
    header: str
    lines: List[str]


@dataclass
class FilePatch:
    file_hint: str
    prelude: List[str]
    hunks: List[Hunk]


def split_file_patches(diff_text: str) -> List[List[str]]:
    lines = diff_text.splitlines(keepends=True)
    chunks: List[List[str]] = []
    current: List[str] = []
    for line in lines:
        if DIFF_START.match(line) and current:
            chunks.append(current)
            current = [line]
        else:
            current.append(line)
    if current:
        chunks.append(current)
    return chunks


def parse_file_patch(lines: List[str]) -> FilePatch:
    prelude: List[str] = []
    hunks: List[Hunk] = []
    current_hunk_header = ""
    current_hunk_lines: List[str] = []

    file_hint = "unknown"
    for ln in lines:
        if ln.startswith("+++ b/"):
            file_hint = ln[len("+++ b/") :].strip()

    in_hunk = False
    for ln in lines:
        if HUNK_START.match(ln):
            if in_hunk:
                hunks.append(Hunk(current_hunk_header, current_hunk_lines))
            current_hunk_header = ln
            current_hunk_lines = []
            in_hunk = True
            continue

        if in_hunk:
            current_hunk_lines.append(ln)
        else:
            prelude.append(ln)

    if in_hunk:
        hunks.append(Hunk(current_hunk_header, current_hunk_lines))

    if not hunks:
        hunks = [Hunk("", [])]

    return FilePatch(file_hint=file_hint, prelude=prelude, hunks=hunks)


def render_patch(file_patch: FilePatch, hunk_order: List[int]) -> str:
    out = "".join(file_patch.prelude)
    for idx in hunk_order:
        h = file_patch.hunks[idx]
        out += h.header
        out += "".join(h.lines)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description="Create shuffled diff passes")
    parser.add_argument("input", nargs="?", help="Input diff file (defaults to stdin)")
    parser.add_argument(
        "-p", "--passes", type=int, default=8, help="Number of shuffled passes"
    )
    parser.add_argument("-s", "--seed", type=int, default=42, help="Base seed")
    parser.add_argument("-o", "--output", help="Output JSON file (defaults to stdout)")
    args = parser.parse_args()

    if args.input:
        with open(args.input, "r", encoding="utf-8") as f:
            diff_text = f.read()
    else:
        diff_text = sys.stdin.read()

    chunk_lines = split_file_patches(diff_text)
    file_patches = [parse_file_patch(chunk) for chunk in chunk_lines]

    passes = []
    indices = list(range(len(file_patches)))
    for i in range(args.passes):
        rng = random.Random(args.seed + i)
        file_order = indices[:]
        rng.shuffle(file_order)

        rendered = []
        manifest = []
        for file_idx in file_order:
            fp = file_patches[file_idx]
            hunk_order = list(range(len(fp.hunks)))
            rng.shuffle(hunk_order)
            rendered.append(render_patch(fp, hunk_order))
            manifest.append(
                {
                    "file": fp.file_hint,
                    "hunk_count": len(fp.hunks),
                    "hunk_order": hunk_order,
                }
            )

        passes.append(
            {
                "pass_id": i + 1,
                "seed": args.seed + i,
                "manifest": manifest,
                "diff_text": "".join(rendered),
            }
        )

    payload = {"passes": passes}
    text = json.dumps(payload, ensure_ascii=False, indent=2)

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
