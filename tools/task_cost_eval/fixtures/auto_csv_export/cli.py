import sys

from data import load_sales
from report import render_text, summarize


def main(argv):
    if len(argv) != 2:
        print('usage: cli.py SALES_JSON', file=sys.stderr)
        return 2
    print(render_text(summarize(load_sales(argv[1]))))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
