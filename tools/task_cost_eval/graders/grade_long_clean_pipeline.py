#!/usr/bin/env python3
import importlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from _par_common import attempt, finish, fixture_dir

EXPECTED = json.loads(Path(__file__).with_name('long_clean_pipeline_expected.json').read_text())


def main(argv):
    root = fixture_dir(argv, 'long_clean_pipeline')
    failures = []
    sys.path.insert(0, str(root))
    for column, cases in sorted(EXPECTED['columns'].items()):
        try:
            module = importlib.import_module(f'normalizers.{column}')
        except Exception as error:
            failures.append(f'{column}: import failed: {error!r}')
            continue
        for case in cases:
            attempt(failures, f"{column}({case['input']!r})", lambda: module.normalize(case['input']),
                    case['expected']['ok'])
    with tempfile.TemporaryDirectory() as tmp:
        raw = Path(tmp) / 'raw.csv'
        raw.write_text(EXPECTED['raw'])
        run = subprocess.run([sys.executable, 'pipeline.py', str(raw), str(Path(tmp) / 'out')], cwd=root,
                             capture_output=True, text=True, timeout=60,
                             env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'})
        attempt(failures, 'pipeline exit status', lambda: (run.returncode, run.stderr[-300:]), (0, ''))
        for name, text in sorted(EXPECTED['pipeline'].items()):
            path = Path(tmp) / 'out' / name
            attempt(failures, f'pipeline {name}', lambda: path.read_text() if path.exists() else None, text)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
