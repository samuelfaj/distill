import unittest

from normalizers.bookings_name import normalize


class BookingsNameTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ana de silva'), 'Ana de Silva')
        self.assertEqual(normalize('ana-luiza moraes'), 'Ana-Luiza Moraes')
