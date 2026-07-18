#!/usr/bin/env python3
"""Byte-equivalence gating for JSONL artifact files."""

import argparse
import json
import sys


def _raw_lines(path):
    with open(path, "rb") as f:
        return f.read().splitlines()


def _try_json(raw):
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError, ValueError):
        return None


def _fmt(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False)


def _field_diff(baseline_obj, candidate_obj):
    """Yield lines describing top-level key differences."""
    if not isinstance(baseline_obj, dict) or not isinstance(candidate_obj, dict):
        if baseline_obj != candidate_obj:
            yield f"(root): {_fmt(baseline_obj)} != {_fmt(candidate_obj)}"
        return
    keys = sorted(set(baseline_obj) | set(candidate_obj))
    for key in keys:
        in_b = key in baseline_obj
        in_c = key in candidate_obj
        if not in_b:
            yield f"{key}: <missing> != {_fmt(candidate_obj[key])}"
        elif not in_c:
            yield f"{key}: {_fmt(baseline_obj[key])} != <missing>"
        elif baseline_obj[key] != candidate_obj[key]:
            yield f"{key}: {_fmt(baseline_obj[key])} != {_fmt(candidate_obj[key])}"


def compare(baseline_path, candidate_path):
    try:
        baseline_lines = _raw_lines(baseline_path)
        candidate_lines = _raw_lines(candidate_path)
    except OSError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    n_b = len(baseline_lines)
    n_c = len(candidate_lines)
    n = min(n_b, n_c)

    for i in range(n):
        b = baseline_lines[i]
        c = candidate_lines[i]
        if b == c:
            continue

        lineno = i + 1
        print(f"DIFF at line {lineno}")
        print(f"baseline: {b!r}")
        print(f"candidate: {c!r}")

        b_obj = _try_json(b)
        c_obj = _try_json(c)
        if b_obj is not None and c_obj is not None:
            for line in _field_diff(b_obj, c_obj):
                print(line)
        return 1

    if n_b != n_c:
        lineno = n + 1
        if n_b > n_c:
            extra = baseline_lines[n]
            print(f"LENGTH MISMATCH: extra line {lineno} in baseline")
            print(f"baseline: {extra!r}")
        else:
            extra = candidate_lines[n]
            print(f"LENGTH MISMATCH: extra line {lineno} in candidate")
            print(f"candidate: {extra!r}")
        return 1

    print(f"IDENTICAL {n_b} lines")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Compare two JSONL artifact files for byte-equivalence."
    )
    parser.add_argument("baseline", help="path to baseline JSONL file")
    parser.add_argument("candidate", help="path to candidate JSONL file")
    try:
        args = parser.parse_args(argv)
    except SystemExit as e:
        # argparse exits 2 on error, 0 on -h; normalize usage errors to 2
        code = e.code
        if code is None or code == 0:
            return 0
        return 2

    return compare(args.baseline, args.candidate)


if __name__ == "__main__":
    sys.exit(main())
