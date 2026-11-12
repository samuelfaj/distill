import unittest

from normalizers.projects_country import normalize


class ProjectsCountryTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('BRAZIL'), 'BR')
        self.assertEqual(normalize('atlantis'), None)
