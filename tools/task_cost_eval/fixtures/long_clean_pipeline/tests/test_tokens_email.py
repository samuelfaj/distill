import unittest

from normalizers.tokens_email import normalize


class TokensEmailTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize(' bob@mail.example.edu '), 'bob@mail.example.edu')
        self.assertEqual(normalize('Ana.Silva@Example.ORG'), None)
