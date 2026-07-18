"""Tests for equivdiff.py byte-equivalence gating CLI."""

import subprocess
import sys
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parent / "equivdiff.py"


def run_equivdiff(baseline: Path, candidate: Path):
    result = subprocess.run(
        [sys.executable, str(SCRIPT), str(baseline), str(candidate)],
        capture_output=True,
        text=True,
    )
    return result.returncode, result.stdout, result.stderr


def write_jsonl(path: Path, lines):
    # lines are str; write as UTF-8 bytes with newlines (no trailing newline after last unless present)
    data = b"\n".join(
        line.encode("utf-8") if isinstance(line, str) else line for line in lines
    )
    if lines:
        data += b"\n"
    path.write_bytes(data)


def test_identical_files(tmp_path):
    baseline = tmp_path / "baseline.jsonl"
    candidate = tmp_path / "candidate.jsonl"
    rows = [
        '{"id": 1, "name": "alpha"}',
        '{"id": 2, "name": "beta"}',
    ]
    write_jsonl(baseline, rows)
    write_jsonl(candidate, rows)

    code, out, _err = run_equivdiff(baseline, candidate)
    assert code == 0
    assert out.strip() == "IDENTICAL 2 lines"


def test_one_differing_json_field(tmp_path):
    baseline = tmp_path / "baseline.jsonl"
    candidate = tmp_path / "candidate.jsonl"
    write_jsonl(
        baseline,
        [
            '{"id": 1, "status": "ok"}',
            '{"id": 2, "status": "ok"}',
        ],
    )
    write_jsonl(
        candidate,
        [
            '{"id": 1, "status": "ok"}',
            '{"id": 2, "status": "fail"}',
        ],
    )

    code, out, _err = run_equivdiff(baseline, candidate)
    assert code == 1
    assert "DIFF at line 2" in out
    assert "baseline:" in out
    assert "candidate:" in out
    assert 'status: "ok" != "fail"' in out


def test_non_json_differing_line(tmp_path):
    baseline = tmp_path / "baseline.jsonl"
    candidate = tmp_path / "candidate.jsonl"
    write_jsonl(baseline, ["not json at all", "still fine"])
    write_jsonl(candidate, ["also not json", "still fine"])

    code, out, _err = run_equivdiff(baseline, candidate)
    assert code == 1
    assert "DIFF at line 1" in out
    assert "baseline:" in out
    assert "candidate:" in out
    # Non-JSON: no per-field breakdown beyond the header lines
    extra = [
        line
        for line in out.splitlines()
        if line and not line.startswith(("DIFF", "baseline:", "candidate:"))
    ]
    assert extra == []


def test_length_mismatch(tmp_path):
    baseline = tmp_path / "baseline.jsonl"
    candidate = tmp_path / "candidate.jsonl"
    write_jsonl(baseline, ['{"n": 1}', '{"n": 2}'])
    write_jsonl(candidate, ['{"n": 1}'])

    code, out, _err = run_equivdiff(baseline, candidate)
    assert code == 1
    assert "LENGTH MISMATCH" in out
    assert "extra line 2 in baseline" in out
    assert "baseline:" in out


def test_empty_vs_empty(tmp_path):
    baseline = tmp_path / "baseline.jsonl"
    candidate = tmp_path / "candidate.jsonl"
    baseline.write_bytes(b"")
    candidate.write_bytes(b"")

    code, out, _err = run_equivdiff(baseline, candidate)
    assert code == 0
    assert out.strip() == "IDENTICAL 0 lines"
