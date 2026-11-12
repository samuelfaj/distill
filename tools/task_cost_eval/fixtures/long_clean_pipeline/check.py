"""Run the tests of one column: python3 check.py normalizers/NAME.py"""
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def main(argv):
    if len(argv) != 2:
        print('usage: python3 check.py normalizers/NAME.py', file=sys.stderr)
        return 2
    name = Path(argv[1]).stem
    return subprocess.run([sys.executable, '-m', 'unittest', f'tests.test_{name}'], cwd=HERE).returncode


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
