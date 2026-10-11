import json
import sys
from pathlib import Path
from replay import replay


def main(argv):
    try:
        data = json.loads(Path(argv[0]).read_text())
        output = replay(data['initial'],data['events'])
    except (ValueError,KeyError,TypeError,OSError,IndexError) as error:
        print(str(error),file=sys.stderr)
        return 2
    print(json.dumps(output,sort_keys=True))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
