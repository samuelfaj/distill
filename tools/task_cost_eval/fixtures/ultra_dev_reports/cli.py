import argparse
import json
import sys
from pathlib import Path
from service import render


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('report')
    parser.add_argument('input')
    parser.add_argument('--as-of')
    args = parser.parse_args()
    try:
        output = render(args.report, json.loads(Path(args.input).read_text()), args.as_of)
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(json.dumps(output, sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
