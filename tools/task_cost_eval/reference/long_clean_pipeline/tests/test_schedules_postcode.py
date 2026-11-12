import unittest

from normalizers.schedules_postcode import normalize


class SchedulesPostcodeTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('123AB4'), '123AB4')
        self.assertEqual(normalize('123 ab 4'), '123AB4')
