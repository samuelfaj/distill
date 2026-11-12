import unittest

from normalizers.stocks_flag import normalize


class StocksFlagTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('JA'), 'true')
        self.assertEqual(normalize('maybe'), None)
