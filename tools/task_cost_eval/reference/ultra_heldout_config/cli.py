import argparse
import json
import sys
from pathlib import Path
from api import resolve
from export import render


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--json', action='append', default=[])
    parser.add_argument('--env')
    parser.add_argument('--format', default='json', choices=('json','env'))
    args = parser.parse_args()
    try:
        env = json.loads(Path(args.env).read_text()) if args.env else None
        output = render(resolve([Path(path).read_text() for path in args.json], env),args.format)
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(output, end='')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
