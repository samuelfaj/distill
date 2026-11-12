import unittest

from normalizers.members_email import normalize


class MembersEmailTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('Ana.Silva@Example.ORG'), None)
        self.assertEqual(normalize('a@b@c.org'), None)
