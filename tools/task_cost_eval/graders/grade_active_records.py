#!/usr/bin/env python3
import importlib.util
import sys
from pathlib import Path


def load_module(path: Path):
    spec = importlib.util.spec_from_file_location("task_cost_active_records_fixture", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(argv):
    if len(argv) != 2:
        print("usage: grade_active_records.py WORKTREE", file=sys.stderr)
        return 2
    module = load_module(
        Path(argv[1]) / "tools/task_cost_eval/fixtures/active_records/records.py"
    )
    records = [
        {"id": "first", "active": False},
        {"id": "second", "active": True},
        {"id": "missing"},
        {"id": "third", "active": True},
        {"id": "truthy", "active": 1},
    ]
    original = [record.copy() for record in records]
    actual = module.active_records(records)
    expected = [records[1], records[3]]
    if actual != expected or actual is records or records != original:
        print("active_records behavior mismatch", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
