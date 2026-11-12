import unittest

from normalizers.reminders_name import normalize


class RemindersNameTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ana da silva'), 'Ana da Silva')
        self.assertEqual(normalize('ana-luiza moraes'), 'Ana-Luiza Moraes')
