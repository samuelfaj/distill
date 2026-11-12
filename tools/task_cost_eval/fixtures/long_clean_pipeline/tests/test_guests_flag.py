import unittest

from normalizers.guests_flag import normalize


class GuestsFlagTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('YES'), 'true')
        self.assertEqual(normalize('maybe'), None)
