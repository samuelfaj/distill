import unittest

from normalizers.labels_country import normalize


class LabelsCountryTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('MEXICO'), 'MX')
        self.assertEqual(normalize('atlantis'), None)
