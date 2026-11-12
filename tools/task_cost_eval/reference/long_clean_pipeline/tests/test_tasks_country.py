import unittest

from normalizers.tasks_country import normalize


class TasksCountryTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('GERMANY'), 'DE')
        self.assertEqual(normalize('new   zealand'), 'NZ')
