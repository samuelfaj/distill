#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from _par_common import attempt, finish, fixture_dir, import_fresh

TASKS = [{'id': 1, 'title': 'old open', 'done': False, 'created': '2026-02-27'},
         {'id': 2, 'title': 'leap done', 'done': True, 'created': '2026-02-28'},
         {'id': 3, 'title': 'march open', 'done': False, 'created': '2026-03-01'},
         {'id': 4, 'title': 'later done', 'done': True, 'created': '2026-11-30'}]


def main(argv):
    root = fixture_dir(argv, 'auto_since_filter')
    failures = []
    try:
        query, = import_fresh(root, 'query')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'filter_tasks unchanged', lambda: [t['id'] for t in query.filter_tasks(TASKS, 'done')], [2, 4])
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / 'tasks.json'
        path.write_text(json.dumps(TASKS))

        def ids(*extra):
            run = subprocess.run([sys.executable, 'cli.py', 'list', str(path), *extra], cwd=root, env=env,
                                 capture_output=True, text=True, timeout=10)
            if run.returncode:
                return run.returncode, bool(run.stderr.strip()), run.stdout
            return [int(line.split()[2 if line.startswith('[ ]') else 1]) for line in run.stdout.splitlines()]
        attempt(failures, 'no filter', lambda: ids(), [1, 2, 3, 4])
        attempt(failures, 'since is inclusive', lambda: ids('--since', '2026-02-28'), [2, 3, 4])
        attempt(failures, 'since with status', lambda: ids('--since', '2026-02-28', '--status', 'open'), [3])
        attempt(failures, 'status before since', lambda: ids('--status', 'done', '--since', '2026-03-01'), [4])
        attempt(failures, 'since after everything', lambda: ids('--since', '2027-01-01'), [])
        attempt(failures, 'invalid date', lambda: ids('--since', '2026-02-30'), (2, True, ''))
        attempt(failures, 'not a date', lambda: ids('--since', 'yesterday'), (2, True, ''))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
