#!/usr/bin/env python3
import csv
import io
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from _par_common import attempt, finish, fixture_dir, import_fresh

ROWS = [{'region': 'West', 'amount_cents': 1250}, {'region': 'North, East', 'amount_cents': 990},
        {'region': 'West', 'amount_cents': 5}, {'region': 'Quote "Q" Zone', 'amount_cents': 100000}]
EXPECTED = [['region', 'total', 'orders'], ['North, East', '9.90', '1'], ['Quote "Q" Zone', '1000.00', '1'],
            ['West', '12.55', '2']]
TEXT = 'North, East: $9.90 in 1 orders\nQuote "Q" Zone: $1000.00 in 1 orders\nWest: $12.55 in 2 orders\n'


def parse(text):
    return [row for row in csv.reader(io.StringIO(text)) if row]


def main(argv):
    root = fixture_dir(argv, 'auto_csv_export')
    failures = []
    try:
        report, = import_fresh(root, 'report')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'render_csv', lambda: parse(report.render_csv(report.summarize(ROWS))), EXPECTED)
    attempt(failures, 'render_csv empty', lambda: parse(report.render_csv([])), [EXPECTED[0]])
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}
    with tempfile.TemporaryDirectory() as tmp:
        sales = Path(tmp) / 'sales.json'
        sales.write_text(json.dumps(ROWS))

        def cli(*extra):
            run = subprocess.run([sys.executable, 'cli.py', str(sales), *extra], cwd=root, env=env,
                                 capture_output=True, text=True, timeout=10)
            return run.returncode, run.stdout
        attempt(failures, 'cli csv', lambda: (lambda r: (r[0], parse(r[1])))(cli('--format', 'csv')), (0, EXPECTED))
        attempt(failures, 'cli text default', lambda: cli(), (0, TEXT))
        attempt(failures, 'cli explicit text', lambda: cli('--format', 'text'), (0, TEXT))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
