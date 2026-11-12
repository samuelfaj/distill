import argparse
import sys

from query import filter_tasks
from store import load


def main(argv):
    parser = argparse.ArgumentParser(prog='cli.py')
    sub = parser.add_subparsers(dest='command', required=True)
    listing = sub.add_parser('list')
    listing.add_argument('file')
    listing.add_argument('--status', choices=('open', 'done'))
    args = parser.parse_args(argv[1:])
    for task in filter_tasks(load(args.file), args.status):
        mark = 'x' if task['done'] else ' '
        print(f"[{mark}] {task['id']} {task['title']} ({task['created']})")
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
