import unittest

from normalizers.refunds_phone import normalize


class RefundsPhoneTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('7036925814'), '+17036925814')
        self.assertEqual(normalize('+1 7036925814'), '+17036925814')
