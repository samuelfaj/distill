import unittest

from normalizers.uploads_postcode import normalize


class UploadsPostcodeTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('1234AB'), '1234AB')
        self.assertEqual(normalize('1234 ab'), '1234AB')
