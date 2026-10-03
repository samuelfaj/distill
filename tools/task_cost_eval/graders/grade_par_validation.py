#!/usr/bin/env python3
import re
import subprocess
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh

MODULES = (("temperature", "test_temperature"), ("cart", "test_cart"))


def main(argv):
    root = fixture_dir(argv, "par_validation")
    temperature, cart = import_fresh(root, "temperature", "cart")
    failures = []
    attempt(failures, "to_fahrenheit(100)", lambda: temperature.to_fahrenheit(100), 212)
    attempt(failures, "to_fahrenheit(-273.15)", lambda: round(temperature.to_fahrenheit(-273.15), 2), -459.67)
    attempt(failures, "to_fahrenheit(-300)", lambda: temperature.to_fahrenheit(-300), raises=ValueError)
    attempt(failures, "to_fahrenheit('1')", lambda: temperature.to_fahrenheit("1"), raises=TypeError)
    attempt(failures, "to_fahrenheit(True)", lambda: temperature.to_fahrenheit(True), raises=TypeError)
    attempt(failures, "add_item ok", lambda: cart.add_item({"pen": 1}, "pen", 2), {"pen": 3})
    for name, qty in (("", 1), ("  ", 1), (None, 1), ("pen", 0), ("pen", -1), ("pen", 1.5), ("pen", True)):
        attempt(failures, f"add_item({name!r},{qty!r})", lambda: cart.add_item({}, name, qty), raises=ValueError)
    for module, tests in MODULES:
        source = (root / f"{tests}.py").read_text()
        if len(re.findall(r"def test_", source)) < 3 or source.count("assertRaises") < 2:
            failures.append(f"{tests}.py needs at least 3 tests, 2 using assertRaises")
        run = subprocess.run([sys.executable, "-m", "unittest", tests], cwd=root, capture_output=True, text=True)
        if run.returncode != 0:
            failures.append(f"{tests} failed: {run.stderr.strip().splitlines()[-1:]}")
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
