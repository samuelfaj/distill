#!/usr/bin/env python3
import importlib.util
import sys
from pathlib import Path


def load_module(path: Path):
    fixture_dir = str(path.parent)
    sys.path.insert(0, fixture_dir)
    try:
        spec = importlib.util.spec_from_file_location("task_cost_inventory_fixture", path)
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot load {path}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    finally:
        sys.path.pop(0)


def main(argv):
    if len(argv) != 2:
        print("usage: grade_inventory_report.py WORKTREE", file=sys.stderr)
        return 2
    module = load_module(
        Path(argv[1]) / "tools/task_cost_eval/fixtures/inventory_report/inventory.py"
    )
    items = [
        {"sku": "B", "quantity": 2},
        {"sku": "A", "quantity": 3},
        {"sku": "A", "quantity": 2},
    ]
    original = [item.copy() for item in items]
    actual = module.render_inventory(items)
    if actual != "A 5\nB 2" or items != original:
        print("inventory report behavior mismatch", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
