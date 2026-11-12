import unittest

from inventory import Inventory, OutOfStock


class InventoryTests(unittest.TestCase):
    def test_reserve(self):
        inventory = Inventory({'pen': 5})
        inventory.reserve('o1', [('pen', 2)])
        self.assertEqual(inventory.available('pen'), 3)

    def test_out_of_stock(self):
        with self.assertRaises(OutOfStock):
            Inventory({'pen': 1}).reserve('o1', [('pen', 2)])
