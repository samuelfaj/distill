#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        limits, = import_fresh(fixture_dir(argv, 'ctrl_clamp'), 'limits')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'above', lambda: limits.clamp(5, 0, 3), 3)
    attempt(failures, 'below', lambda: limits.clamp(-1, 0, 3), 0)
    attempt(failures, 'inside', lambda: limits.clamp(2, 0, 3), 2)
    attempt(failures, 'bounds', lambda: (limits.clamp(0, 0, 3), limits.clamp(3, 0, 3)), (0, 3))
    attempt(failures, 'floats', lambda: limits.clamp(1.5, 0.0, 1.25), 1.25)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
