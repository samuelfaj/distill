import unittest

from limits import clamp


class ClampTests(unittest.TestCase):
    def test_inside(self):
        self.assertEqual(clamp(2, 0, 3), 2)

    def test_below(self):
        self.assertEqual(clamp(-1, 0, 3), 0)
