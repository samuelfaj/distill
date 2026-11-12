#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        paging, = import_fresh(fixture_dir(argv, 'auto_paging_fix'), 'paging')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    items = list(range(10))
    attempt(failures, 'first page', lambda: paging.page(items, 1, 3), [0, 1, 2])
    attempt(failures, 'last partial page', lambda: paging.page(items, 4, 3), [9])
    attempt(failures, 'past the end', lambda: paging.page(items, 5, 3), [])
    attempt(failures, 'tuple input', lambda: paging.page(tuple(items), 2, 5), [5, 6, 7, 8, 9])
    attempt(failures, 'page zero rejected', lambda: paging.page(items, 0, 3), raises=ValueError)
    attempt(failures, 'count', lambda: paging.page_count(10, 3), 4)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
