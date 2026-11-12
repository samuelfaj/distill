import unittest

from normalizers.coupons_phone import normalize


class CouponsPhoneTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('+49 70369258147'), '+4970369258147')
        self.assertEqual(normalize('004970369258147'), '+4970369258147')
