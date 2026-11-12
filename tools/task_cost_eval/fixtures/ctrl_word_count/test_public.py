import unittest

from words import count_words


class WordTests(unittest.TestCase):
    def test_counts(self):
        self.assertEqual(count_words('a b a'), {'a': 2, 'b': 1})
