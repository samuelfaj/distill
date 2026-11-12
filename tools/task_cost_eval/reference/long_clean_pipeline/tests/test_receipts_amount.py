import unittest

from normalizers.receipts_amount import normalize


class ReceiptsAmountTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('1,234.50'), '1234.500')
        self.assertEqual(normalize('1.0005'), '1.001')
