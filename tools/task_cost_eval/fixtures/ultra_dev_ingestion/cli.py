import json
import sys
from pathlib import Path
from api import combine
from errors import FeedError


def main(argv):
    try:
        feeds = []
        for spec in argv:
            fmt, path = spec.split(':', 1)
            feeds.append((fmt, Path(path).read_text()))
        print(json.dumps(combine(feeds), sort_keys=True))
    except (FeedError, OSError, ValueError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
