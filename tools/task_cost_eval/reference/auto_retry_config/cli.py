import json
import sys

from client import policy_for
from endpoints import ENDPOINTS


def main(argv):
    if argv[1:] == ['list']:
        print('\n'.join(ENDPOINTS))
        return 0
    if len(argv) == 3 and argv[1] == 'show-policy':
        attempts, backoff = policy_for(argv[2])
        print(json.dumps({'attempts': attempts, 'backoff_s': backoff}))
        return 0
    print('usage: cli.py list | cli.py show-policy ENDPOINT', file=sys.stderr)
    return 2


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
