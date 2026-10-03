"""Shared helpers for the parallel-cohort graders."""
import importlib
import sys
from pathlib import Path


def fixture_dir(argv, name):
    if len(argv) != 2:
        print(f"usage: grade_{name}.py WORKTREE", file=sys.stderr)
        raise SystemExit(2)
    return Path(argv[1]) / "tools/task_cost_eval/fixtures" / name


def import_fresh(directory, *names):
    """Import sibling modules from directory, discarding any cached copies."""
    sys.path.insert(0, str(directory))
    try:
        for name in names:
            sys.modules.pop(name, None)
        return [importlib.import_module(name) for name in names]
    finally:
        sys.path.remove(str(directory))


def attempt(failures, label, call, expected=None, raises=None):
    try:
        actual = call()
    except BaseException as error:
        if raises is None or not isinstance(error, raises):
            failures.append(f"{label}: raised {type(error).__name__}: {error}")
        return
    if raises is not None:
        failures.append(f"{label}: expected {raises.__name__}, got {actual!r}")
    elif actual != expected:
        failures.append(f"{label}: {actual!r} != {expected!r}")


def finish(failures):
    for failure in failures:
        print(failure, file=sys.stderr)
    return 1 if failures else 0
