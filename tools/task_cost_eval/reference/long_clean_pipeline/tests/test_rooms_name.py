import unittest

from normalizers.rooms_name import normalize


class RoomsNameTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ana di silva'), 'Ana di Silva')
        self.assertEqual(normalize('DI costa'), 'Di Costa')
