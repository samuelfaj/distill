"""Check one handler module: python3 check.py handlers/NAME.py"""
import importlib
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import framework  # noqa: E402


def main(argv):
    if len(argv) != 2:
        print('usage: python3 check.py handlers/NAME.py', file=sys.stderr)
        return 2
    module = f'handlers.{Path(argv[1]).stem}'
    importlib.import_module(module)
    handlers = framework.handlers_in(module)
    if not handlers:
        print(f'{module}: no routes registered with route()', file=sys.stderr)
        return 1
    failed = False
    for name, handler in sorted(handlers.items()):
        result = handler({'id': 1, 'qty': 2})
        if not isinstance(result, framework.Response):
            print(f'{name}: returned {type(result).__name__}, expected Response', file=sys.stderr)
            failed = True
    if not failed:
        print(f'ok {module}: {", ".join(sorted(handlers))}')
    return 1 if failed else 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
