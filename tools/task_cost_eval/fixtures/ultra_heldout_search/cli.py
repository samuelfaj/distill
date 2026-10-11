import argparse
import json
import sys
from pathlib import Path
from api import query


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('input')
    parser.add_argument('--query', default='')
    parser.add_argument('--tag', action='append', default=[])
    parser.add_argument('--min-rating', type=int)
    parser.add_argument('--include-archived', action='store_true')
    parser.add_argument('--offset', type=int, default=0)
    parser.add_argument('--limit', type=int)
    args = parser.parse_args()
    try:
        output = query(json.loads(Path(args.input).read_text()), args.query, args.tag,
                       args.min_rating, args.include_archived, args.offset, args.limit)
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(json.dumps(output, sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
