import argparse
import sys

from data import load_sales
from report import render_csv, render_text, summarize


def main(argv):
    parser = argparse.ArgumentParser(prog='cli.py')
    parser.add_argument('sales')
    parser.add_argument('--format', choices=('text', 'csv'), default='text')
    args = parser.parse_args(argv[1:])
    summary = summarize(load_sales(args.sales))
    if args.format == 'csv':
        sys.stdout.write(render_csv(summary))
    else:
        print(render_text(summary))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
