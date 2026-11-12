import unittest

from normalizers.alerts_amount import normalize


class AlertsAmountTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('(12.00)'), '-12.0')
        self.assertEqual(normalize('1,234.50'), '1234.5')
