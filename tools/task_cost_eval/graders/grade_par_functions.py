#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv, "par_functions")
    slug, ranges, roman = import_fresh(root, "slug", "ranges", "roman")
    failures = []
    for text, expected in (("Hello, World!", "hello-world"), ("  A  b--c ", "a-b-c"), ("***", "")):
        attempt(failures, f"slugify({text!r})", lambda: slug.slugify(text), expected)
    for value, expected in (([(6, 7), (1, 3), (2, 4)], [(1, 4), (6, 7)]), ([(1, 2), (3, 4)], [(1, 4)]),
                            ([(1, 9), (2, 3)], [(1, 9)]), ([(1, 2), (4, 5)], [(1, 2), (4, 5)]), ([], [])):
        attempt(failures, f"merge_ranges({value!r})", lambda: ranges.merge_ranges(list(value)), expected)
    for number, expected in ((1, "I"), (4, "IV"), (1994, "MCMXCIV"), (3999, "MMMCMXCIX")):
        attempt(failures, f"to_roman({number})", lambda: roman.to_roman(number), expected)
    for number in (0, 4000, -1):
        attempt(failures, f"to_roman({number})", lambda: roman.to_roman(number), raises=ValueError)
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
