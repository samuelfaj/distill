import unittest

from fmt import format_amount
from report import summary_line


class FormatTests(unittest.TestCase):
    def test_plain(self):
        self.assertEqual(format_amount(1234.5), '1,234.50')

    def test_currency(self):
        self.assertEqual(format_amount(1234.5, currency='USD'), '$1,234.50')
        self.assertEqual(format_amount(12, currency='CHF'), 'CHF 12.00')

    def test_summary(self):
        self.assertEqual(summary_line('Total', 1234.6), 'Total: 1,235')
