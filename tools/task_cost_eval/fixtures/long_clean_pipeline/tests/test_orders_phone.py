import unittest

from normalizers.orders_phone import normalize


class OrdersPhoneTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('005570369258147'), '+5570369258147')
        self.assertEqual(normalize('+55 70369258147'), '+5570369258147')
