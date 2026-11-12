import unittest

from normalizers.imports_postcode import normalize


class ImportsPostcodeTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('c1425 abc'), 'C1425ABC')
        self.assertEqual(normalize('C1425ABCD'), None)
