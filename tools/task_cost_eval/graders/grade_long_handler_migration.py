#!/usr/bin/env python3
import importlib
import json
import sys
from pathlib import Path

from _par_common import attempt, finish, fixture_dir

EXPECTED = json.loads(Path(__file__).with_name('long_handler_migration_expected.json').read_text())


def main(argv):
    root = fixture_dir(argv, 'long_handler_migration')
    failures = []
    sys.path.insert(0, str(root))
    try:
        framework = importlib.import_module('framework')
        importlib.import_module('app').load_all()
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'register() removed', lambda: hasattr(framework, 'register'), False)
    attempt(failures, 'routes and allowed methods unchanged', framework.routes, EXPECTED['routes'])
    for path in sorted((root / 'handlers').glob('*.py')):
        if path.name == '__init__.py':
            continue
        module = sys.modules.get(f'handlers.{path.stem}')
        for function in ('_get', '_create'):
            if module is not None and hasattr(module, function):
                attempt(failures, f'{path.stem}.{function} returns a Response',
                        lambda: isinstance(getattr(module, function)({'id': 1, 'qty': 2}), framework.Response), True)
    for call in EXPECTED['calls']:
        def respond(call=call):
            response = framework.dispatch(call['route'], call['method'], dict(call['request']))
            return {'status': response.status, 'body': response.body, 'headers': response.headers}
        attempt(failures, f"{call['route']} {call['method']} {call['request']}", respond, call['response'])
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
