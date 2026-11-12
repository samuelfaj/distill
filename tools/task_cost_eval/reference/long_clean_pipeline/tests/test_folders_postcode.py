import unittest

from normalizers.folders_postcode import normalize


class FoldersPostcodeTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('h3z 2y7'), 'H3Z2Y7')
        self.assertEqual(normalize('H3Z 2Y7X'), None)
