import unittest

from normalizers.reports_email import normalize


class ReportsEmailTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize(' bob@mail.example.edu '), 'bob@mail.example.edu')
        self.assertEqual(normalize('Ana.Silva@Example.ORG'), 'Ana.Silva@example.org')
