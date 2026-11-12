import unittest

from normalizers.exports_postcode import normalize


class ExportsPostcodeTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('J9 3TT'), 'J93TT')
        self.assertEqual(normalize('qe4b 7nn'), 'QE4B7NN')
