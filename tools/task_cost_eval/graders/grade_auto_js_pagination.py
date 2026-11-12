#!/usr/bin/env python3
import sys

from _js_common import run_node
from _par_common import attempt, finish, fixture_dir

CHECKS = """
const { handle } = require('./router');
const shape = (r) => ({status: r.status, ids: (r.body.items || []).map((i) => i.id),
                       total: r.body.total, nextOffset: r.body.nextOffset});
check('default', () => shape(handle('GET', '/products', {})));
check('second page', () => shape(handle('GET', '/products', {limit: '5', offset: '10'})));
check('last page', () => shape(handle('GET', '/products', {limit: '50', offset: '50'})));
check('capped', () => shape(handle('GET', '/products', {limit: '500'})).ids.length);
check('numbers accepted', () => shape(handle('GET', '/products', {limit: 3, offset: 0})));
check('filtered total', () => shape(handle('GET', '/products', {category: 'books', limit: '30'})));
check('negative', () => handle('GET', '/products', {limit: '-1'}).status);
check('text', () => handle('GET', '/products', {offset: 'abc'}).status);
check('fraction', () => handle('GET', '/products', {limit: '2.5'}).status);
check('item shape', () => handle('GET', '/products', {limit: '1'}).body.items[0]);
"""


def main(argv):
    root = fixture_dir(argv, 'auto_js_pagination')
    failures = []
    try:
        out = run_node(root, CHECKS)
    except Exception as error:
        return finish(['import: ' + repr(error)])
    books = [n for n in range(1, 74) if n % 3 == 0]
    attempt(failures, 'default page', lambda: out['default'],
            {'status': 200, 'ids': list(range(1, 11)), 'total': 73, 'nextOffset': 10})
    attempt(failures, 'limit and offset', lambda: out['second page'],
            {'status': 200, 'ids': list(range(11, 16)), 'total': 73, 'nextOffset': 15})
    attempt(failures, 'last page', lambda: out['last page'],
            {'status': 200, 'ids': list(range(51, 74)), 'total': 73, 'nextOffset': None})
    attempt(failures, 'limit capped at 50', lambda: out['capped'], 50)
    attempt(failures, 'numeric params', lambda: out['numbers accepted'],
            {'status': 200, 'ids': [1, 2, 3], 'total': 73, 'nextOffset': 3})
    attempt(failures, 'category filter with total', lambda: out['filtered total'],
            {'status': 200, 'ids': books[:24], 'total': 24, 'nextOffset': None})
    for label in ('negative', 'text', 'fraction'):
        attempt(failures, f'{label} is a 400', lambda: out[label], 400)
    attempt(failures, 'item shape unchanged', lambda: out['item shape'],
            {'id': 1, 'name': 'Product 1', 'category': 'games', 'price': 1})
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
