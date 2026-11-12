import unittest

from normalizers.files_country import normalize


class FilesCountryTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('SPAIN'), 'ES')
        self.assertEqual(normalize('sri   lanka'), 'LK')
