import unittest

from normalizers.shipments_date import normalize


class ShipmentsDateTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('2026-03-04'), '2026-03-04')
        self.assertEqual(normalize(' 2026-12-31 '), '2026-12-31')
