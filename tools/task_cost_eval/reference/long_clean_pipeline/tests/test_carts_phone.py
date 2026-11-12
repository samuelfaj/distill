import unittest

from normalizers.carts_phone import normalize


class CartsPhoneTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('00351703692581'), '+351703692581')
        self.assertEqual(normalize('+351 703692581'), '+351703692581')
