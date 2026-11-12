import unittest

from normalizers.dishes_flag import normalize


class DishesFlagTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize(' off '), 'false')
        self.assertEqual(normalize('ON'), 'true')
