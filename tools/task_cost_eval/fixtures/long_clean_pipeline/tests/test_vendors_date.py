import unittest

from normalizers.vendors_date import normalize


class VendorsDateTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('2026-03-04'), '2026-03-04')
        self.assertEqual(normalize('4/3/2026'), '2026-03-04')
