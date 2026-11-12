"""Exercise one billing module: python3 check.py billing/NAME.py"""
import importlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import money  # noqa: E402


def main(argv):
    if len(argv) != 2:
        print('usage: python3 check.py billing/NAME.py', file=sys.stderr)
        return 2
    try:
        sample = money.Money('USD', 1)
    except TypeError:
        sample = None
    if sample is None or getattr(sample, 'cents', None) != 1 or hasattr(sample, 'amount_cents'):
        print('money.Money is not migrated yet: expected Money(currency, cents) with a cents attribute',
              file=sys.stderr)
        return 1
    name = Path(argv[1]).stem
    module = importlib.import_module(f'billing.{name}')
    calls = json.loads((HERE / 'samples.json').read_text())[name]
    for function, arguments in calls:
        print(f'{function}{tuple(arguments)!r} -> {getattr(module, function)(*arguments)!r}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
