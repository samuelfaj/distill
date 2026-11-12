#!/usr/bin/env python3
import sys

from _js_common import run_node
from _par_common import attempt, finish, fixture_dir

CASES = {0: '0s', 59499: '59s', 61000: '1m 1s', 119600: '2m 0s', 3599500: '1h 0m 0s', 3725000: '1h 2m 5s'}


def main(argv):
    root = fixture_dir(argv, 'auto_js_duration')
    failures = []
    body = "const { formatDuration } = require('./duration');\n" + ''.join(
        f"check('{ms}', () => formatDuration({ms}));\n" for ms in CASES)
    try:
        out = run_node(root, body)
    except Exception as error:
        return finish(['import: ' + repr(error)])
    for ms, expected in CASES.items():
        attempt(failures, f'formatDuration({ms})', lambda: out[str(ms)], expected)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
