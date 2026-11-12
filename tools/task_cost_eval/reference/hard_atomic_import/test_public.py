import unittest

from importer import import_batch
from store import Store


def item(sku, qty=1, price=100):
    return {'sku': sku, 'qty': qty, 'price_cents': price}


class ImportTests(unittest.TestCase):
    def test_valid_batch(self):
        store = Store()
        self.assertEqual(import_batch(store, [item('a'), item('b')]), {'imported': 2, 'unchanged': 0, 'errors': []})
        self.assertEqual(store.get('a'), item('a'))

    def test_invalid_records_store_nothing(self):
        store = Store()
        result = import_batch(store, [item('a'), item('b', qty=-1), {'sku': ''}])
        self.assertEqual(result['imported'], 0)
        self.assertEqual([index for index, _ in result['errors']], [1, 2])
        self.assertEqual(store.snapshot(), {})

    def test_unchanged(self):
        store = Store()
        import_batch(store, [item('a')])
        self.assertEqual(import_batch(store, [item('a'), item('b')]), {'imported': 1, 'unchanged': 1, 'errors': []})
