import unittest

from normalizers.tickets_amount import normalize


class TicketsAmountTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('1,234.50'), '1234.50')
        self.assertEqual(normalize('(12.00)'), '-12.00')
