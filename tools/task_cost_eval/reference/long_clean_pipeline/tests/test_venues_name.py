import unittest

from normalizers.venues_name import normalize


class VenuesNameTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ana von silva'), 'Ana von Silva')
        self.assertEqual(normalize('MARIA   zu  souza'), 'Maria zu Souza')
