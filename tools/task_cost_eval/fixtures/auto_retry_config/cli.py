import sys

from endpoints import ENDPOINTS


def main(argv):
    if argv[1:] == ['list']:
        print('\n'.join(ENDPOINTS))
        return 0
    print('usage: cli.py list', file=sys.stderr)
    return 2


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
