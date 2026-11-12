import unittest

from normalizers.sessions_email import normalize


class SessionsEmailTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('Ana.Silva@Example.ORG'), 'Ana.Silva@example.org')
        self.assertEqual(normalize('a@b@c.org'), None)
