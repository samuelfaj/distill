import unittest

from api import list_items


class ApiTests(unittest.TestCase):
    def test_first_page(self):
        store = {'items': [f'item-{n:03}' for n in range(30)], 'users': []}
        self.assertEqual(len(list_items(store)), 20)
