#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def walk(feed, data, limit, between=None):
    """Ids in the order a reader paging through the feed sees them; between(data, page) runs after each page."""
    seen, cursor, pages = [], None, 0
    while pages < 50:
        result = feed.page(data, cursor, limit=limit)
        seen += [post['id'] for post in result['items']]
        cursor = result['next_cursor']
        pages += 1
        if cursor is None:
            break
        if between:
            between(data, result)
    return seen


def main(argv):
    failures = []
    try:
        feed, = import_fresh(fixture_dir(argv, 'hard_cursor_pages'), 'feed')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    # Many posts share a timestamp, across page boundaries.
    tied = [{'id': n, 'ts': 1000 + n // 4} for n in range(1, 31)]
    expected = [post['id'] for post in sorted(tied, key=lambda p: (p['ts'], p['id']), reverse=True)]
    attempt(failures, 'ties across page boundaries', lambda: walk(feed, list(tied), 3), expected)
    attempt(failures, 'ties with another page size', lambda: walk(feed, list(tied), 7), expected)

    def insert_on_top(data, result):
        data.append({'id': 100 + len(data), 'ts': 5000 + len(data)})
    attempt(failures, 'new posts on top while paging', lambda: walk(feed, list(tied), 5, insert_on_top), expected)

    def delete_last_served(data, result):
        last = result['items'][-1]['id']
        data[:] = [post for post in data if post['id'] != last]
    # Every post is still seen exactly once: the deleted ones had already been served.
    attempt(failures, 'the post a cursor points at is deleted',
            lambda: walk(feed, list(tied), 5, delete_last_served), expected)

    def delete_unseen(data, result):
        data[:] = [post for post in data if post['id'] != 1]
    attempt(failures, 'a post not yet seen is deleted', lambda: walk(feed, list(tied), 4, delete_unseen),
            [i for i in expected if i != 1])

    def exact_multiple():
        data = [{'id': n, 'ts': n} for n in range(1, 11)]
        first = feed.page(data, None, limit=5)
        second = feed.page(data, first['next_cursor'], limit=5)
        return [p['id'] for p in second['items']], second['next_cursor']
    attempt(failures, 'last full page has no next cursor', exact_multiple, ([5, 4, 3, 2, 1], None))
    attempt(failures, 'malformed cursor', lambda: feed.page(tied, 'not a cursor!', limit=5), raises=ValueError)
    attempt(failures, 'cursor is a string', lambda: type(feed.page(tied, None, limit=5)['next_cursor']).__name__, 'str')
    attempt(failures, 'empty feed', lambda: feed.page([], None, limit=5), {'items': [], 'next_cursor': None})
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
