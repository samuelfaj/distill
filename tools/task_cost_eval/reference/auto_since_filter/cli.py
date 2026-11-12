import argparse
import datetime
import sys

from query import created_since, filter_tasks
from store import load


def main(argv):
    parser = argparse.ArgumentParser(prog='cli.py')
    sub = parser.add_subparsers(dest='command', required=True)
    listing = sub.add_parser('list')
    listing.add_argument('file')
    listing.add_argument('--status', choices=('open', 'done'))
    listing.add_argument('--since')
    args = parser.parse_args(argv[1:])
    tasks = filter_tasks(load(args.file), args.status)
    if args.since is not None:
        try:
            since = datetime.date.fromisoformat(args.since)
        except ValueError:
            print(f'invalid --since date: {args.since}', file=sys.stderr)
            return 2
        tasks = created_since(tasks, since)
    for task in tasks:
        mark = 'x' if task['done'] else ' '
        print(f"[{mark}] {task['id']} {task['title']} ({task['created']})")
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
