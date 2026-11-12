import unittest

from normalizers.events_name import normalize


class EventsNameTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ana van silva'), 'Ana van Silva')
        self.assertEqual(normalize('VAN costa'), 'Van Costa')
