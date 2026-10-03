#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv, "par_bugs")
    pager, money, words = import_fresh(root, "pager", "money", "words")
    failures = []
    for total, per, expected in ((0, 10, 0), (10, 10, 1), (11, 10, 2), (25, 10, 3)):
        attempt(failures, f"page_count({total},{per})", lambda: pager.page_count(total, per), expected)
    for cents, parts, expected in ((100, 3, [34, 33, 33]), (10, 5, [2] * 5), (2, 3, [1, 1, 0])):
        attempt(failures, f"split_evenly({cents},{parts})", lambda: money.split_evenly(cents, parts), expected)
    for text, expected in (("The the THE cat", "the"), ("b a b a", "a"), ("", None), ("x Y y", "y")):
        attempt(failures, f"top_word({text!r})", lambda: words.top_word(text), expected)
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
