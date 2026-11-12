import unittest

from app import font_label


class AppTests(unittest.TestCase):
    def test_label(self):
        self.assertEqual(font_label({'version': 1, 'font': 'Mono 12', 'plugins': ''}), 'Mono 12pt')
