import argparse
import json
import sys
from pathlib import Path
from catalog import Catalog
from rates import RateTable
from service import QuoteService
from invoice import build


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('input')
    parser.add_argument('--currency', default='USD')
    parser.add_argument('--note', default='')
    args = parser.parse_args()
    try:
        data = json.loads(Path(args.input).read_text())
        service = QuoteService(Catalog(data['catalog']), RateTable(data['rates']))
        output = build(service, data['order'], args.currency, args.note)
    except (ValueError, KeyError, OSError, TypeError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(json.dumps(output, sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
