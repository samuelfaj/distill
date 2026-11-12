import unittest

from catalog import Catalog
from service import Shop


class ShopTests(unittest.TestCase):
    def test_quote(self):
        shop = Shop(Catalog({'pen': 150, 'pad': 300}))
        self.assertEqual(shop.quote([('pen', 2), ('pad', 1)]), 600)

    def test_repeated_quotes_use_the_cache(self):
        catalog = Catalog({'pen': 150})
        shop = Shop(catalog)
        shop.quote([('pen', 1)])
        shop.quote([('pen', 1)])
        self.assertEqual(catalog.lookups, 1)
