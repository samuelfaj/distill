#!/usr/bin/env python3
import importlib.util
import sys
from pathlib import Path


def load_module(path: Path):
    spec = importlib.util.spec_from_file_location("task_cost_tax_fixture", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(argv):
    if len(argv) != 2:
        print("usage: grade_tax_bug.py WORKTREE", file=sys.stderr)
        return 2
    module = load_module(Path(argv[1]) / "tools/task_cost_eval/fixtures/tax_bug/tax.py")
    checks = ((10000, 825, 10825), (1000, 0, 1000), (999, 333, 1032))
    for amount, rate, expected in checks:
        actual = module.total_with_tax(amount, rate)
        if actual != expected:
            print(f"tax behavior mismatch for {amount}/{rate}: {actual} != {expected}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
