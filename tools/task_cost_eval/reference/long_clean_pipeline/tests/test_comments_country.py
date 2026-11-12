import unittest

from normalizers.comments_country import normalize


class CommentsCountryTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('jp'), 'JP')
        self.assertEqual(normalize('JAPAN'), 'JP')
