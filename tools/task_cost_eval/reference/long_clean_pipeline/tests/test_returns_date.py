import unittest

from normalizers.returns_date import normalize


class ReturnsDateTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('2026-03-04'), '2026-03-04')
        self.assertEqual(normalize('13/1/2026'), None)
