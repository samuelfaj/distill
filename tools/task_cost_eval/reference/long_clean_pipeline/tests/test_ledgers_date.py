import unittest

from normalizers.ledgers_date import normalize


class LedgersDateTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('2026-03-04'), '2026-03-04')
        self.assertEqual(normalize('1/13/2026'), None)
