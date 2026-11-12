import unittest

from normalizers.quotes_amount import normalize


class QuotesAmountTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('(12.00)'), '-12')
        self.assertEqual(normalize('1,234.50'), '1235')
