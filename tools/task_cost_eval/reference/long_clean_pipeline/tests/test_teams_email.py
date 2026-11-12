import unittest

from normalizers.teams_email import normalize


class TeamsEmailTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize(' bob@mail.example.edu '), 'bob@mail.example.edu')
        self.assertEqual(normalize('Ana.Silva@Example.ORG'), 'Ana.Silva@example.org')
