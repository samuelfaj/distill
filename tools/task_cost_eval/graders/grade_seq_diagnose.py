#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv, "seq_diagnose")
    parse, report = import_fresh(root, "parse", "report")
    failures = []
    for text, expected in (("19.99", 1999), ("0.29", 29), ("1,000.10", 100010), ("-5.10", -510),
                           ("7", 700), ("0.07", 7)):
        attempt(failures, f"parse_amount({text!r})", lambda: parse.parse_amount(text), expected)
    sample = ["19.99", "0.29", "1,000.10"]
    attempt(failures, "total_cents", lambda: report.total_cents(sample), 102038)
    attempt(failures, "format_total", lambda: report.format_total(sample), "1020.38")
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
