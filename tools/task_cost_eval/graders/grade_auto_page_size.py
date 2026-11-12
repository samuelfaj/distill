#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        defaults, api = import_fresh(fixture_dir(argv, 'auto_page_size'), 'defaults', 'api')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    items = [f'item-{n:03}' for n in range(250)]
    users = [f'user-{n:03}' for n in range(250)]
    store = {'items': list(reversed(items)), 'users': list(reversed(users))}
    attempt(failures, 'limits unchanged', lambda: (defaults.LIMITS['page_size'], defaults.LIMITS['max_page_size']),
            (20, 100))
    attempt(failures, 'default size', lambda: api.list_items(store), items[:20])
    attempt(failures, 'positional page still works', lambda: api.list_items(store, 2), items[20:40])
    attempt(failures, 'custom size', lambda: api.list_items(store, page=2, page_size=5), items[5:10])
    attempt(failures, 'custom size users', lambda: api.list_users(store, page=3, page_size=7), users[14:21])
    attempt(failures, 'capped at max', lambda: len(api.list_items(store, page_size=1000)), 100)
    attempt(failures, 'cap applies to users', lambda: len(api.list_users(store, 1, page_size=101)), 100)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
