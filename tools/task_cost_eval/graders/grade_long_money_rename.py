#!/usr/bin/env python3
import importlib
import json
import sys
from pathlib import Path

from _par_common import attempt, finish, fixture_dir

EXPECTED = json.loads(Path(__file__).with_name('long_money_rename_expected.json').read_text())


def main(argv):
    root = fixture_dir(argv, 'long_money_rename')
    failures = []
    sys.path.insert(0, str(root))
    try:
        money = importlib.import_module('money')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    Money = money.Money
    attempt(failures, 'Money(currency, cents)', lambda: (Money('EUR', 5).currency, Money('EUR', 5).cents), ('EUR', 5))
    attempt(failures, 'keywords', lambda: Money(currency='BRL', cents=7).cents, 7)
    attempt(failures, 'currency is required', lambda: Money(5), raises=TypeError)
    attempt(failures, 'amount_cents attribute is gone', lambda: hasattr(Money('USD', 1), 'amount_cents'), False)
    attempt(failures, 'cents must be an int', lambda: Money('USD', 1.5), raises=TypeError)
    attempt(failures, 'addition', lambda: (Money('USD', 2) + Money('USD', 3)).cents, 5)
    attempt(failures, 'wire format kept', lambda: Money('JPY', 9).to_dict(), {'amount_cents': 9, 'currency': 'JPY'})
    attempt(failures, 'wire format read', lambda: Money.from_dict({'amount_cents': 4, 'currency': 'EUR'}) == Money('EUR', 4),
            True)
    for call in EXPECTED['calls']:
        def result(call=call):
            module = importlib.import_module(f"billing.{call['module']}")
            return getattr(module, call['function'])(*call['arguments'])
        attempt(failures, f"{call['module']}.{call['function']}{tuple(call['arguments'])!r}", result, call['result'])
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
