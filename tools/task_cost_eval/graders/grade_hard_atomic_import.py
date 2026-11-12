#!/usr/bin/env python3
import copy
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def item(sku, qty=1, price=100):
    return {'sku': sku, 'qty': qty, 'price_cents': price}


def main(argv):
    failures = []
    try:
        store_module, importer = import_fresh(fixture_dir(argv, 'hard_atomic_import'), 'store', 'importer')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    Store, StoreFull = store_module.Store, store_module.StoreFull

    class CountingStore(Store):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.puts = 0

        def put(self, key, record):
            self.puts += 1
            super().put(key, record)

    def all_errors():
        store = Store()
        store.put('keep', item('keep'))
        result = importer.import_batch(store, [item('a'), 'bad', item('b', price=0), item('c'), {'sku': 'd'}])
        return result['imported'], [index for index, _ in result['errors']], result['errors'][1][1], store.snapshot()
    attempt(failures, 'every invalid record listed and nothing stored', all_errors,
            (0, [1, 2, 4], 'price_cents must be a positive integer', {'keep': item('keep')}))

    def refused_by_store():
        store = Store(max_items=3)
        store.put('a', item('a', qty=1))
        store.put('b', item('b', qty=1))
        before = store.snapshot()
        try:
            importer.import_batch(store, [item('a', qty=9), item('c'), item('b', qty=7), item('d')])
        except StoreFull:
            return 'StoreFull', store.snapshot() == before, store.snapshot()
        return 'no error', store.snapshot() == before, store.snapshot()
    attempt(failures, 'store refusal restores the store exactly', refused_by_store,
            ('StoreFull', True, {'a': item('a', qty=1), 'b': item('b', qty=1)}))

    def refusal_then_retry():
        store = Store(max_items=2)
        try:
            importer.import_batch(store, [item('a'), item('b'), item('c')])
        except StoreFull:
            pass
        store.max_items = 3
        return importer.import_batch(store, [item('a'), item('b'), item('c')]), sorted(store.snapshot())
    attempt(failures, 'retry after a refusal imports everything', refusal_then_retry,
            ({'imported': 3, 'unchanged': 0, 'errors': []}, ['a', 'b', 'c']))

    def unchanged_not_written():
        store = CountingStore()
        importer.import_batch(store, [item('a'), item('b')])
        store.puts = 0
        result = importer.import_batch(store, [item('a'), item('b', qty=5), item('c')])
        return result, store.puts, store.get('b')
    attempt(failures, 'unchanged records are not written again', unchanged_not_written,
            ({'imported': 2, 'unchanged': 1, 'errors': []}, 2, item('b', qty=5)))

    def input_untouched():
        records = [item('a'), item('b')]
        saved = copy.deepcopy(records)
        store = Store()
        importer.import_batch(store, records)
        records[0]['qty'] = 99
        return records[1:] == saved[1:], store.get('a')
    attempt(failures, 'stored records are independent of the input', input_untouched, (True, item('a')))
    attempt(failures, 'empty batch', lambda: importer.import_batch(Store(), []),
            {'imported': 0, 'unchanged': 0, 'errors': []})
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
