#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv, 'auto_rename_setting')
    failures = [f'{path.name} still mentions retry_limit' for path in sorted(root.glob('*.py'))
                if 'retry_limit' in path.read_text()]
    try:
        settings, client, worker = import_fresh(root, 'settings', 'client', 'worker')
    except Exception as error:
        return finish(failures + ['import: ' + repr(error)])
    attempt(failures, 'default max_retries', lambda: settings.load()['max_retries'], 3)
    attempt(failures, 'override', lambda: settings.load({'max_retries': 5})['max_retries'], 5)
    attempt(failures, 'timeout unchanged', lambda: settings.load()['timeout_s'], 10)
    attempt(failures, 'attempts', lambda: client.attempts_for(settings.load()), 4)
    attempt(failures, 'describe', lambda: worker.describe(settings.load({'max_retries': 2})), 'retries=2 attempts=3')
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
