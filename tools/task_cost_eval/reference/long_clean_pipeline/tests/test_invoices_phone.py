import unittest

from normalizers.invoices_phone import normalize


class InvoicesPhoneTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('+44 7036925814'), '+447036925814')
        self.assertEqual(normalize('00447036925814'), '+447036925814')
