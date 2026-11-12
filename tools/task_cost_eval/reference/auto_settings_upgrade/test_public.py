import unittest

from app import font_label


class AppTests(unittest.TestCase):
    def test_label(self):
        self.assertEqual(font_label({'version': 2, 'font': {'family': 'Mono', 'size': 12}, 'plugins': []}),
                         'Mono 12pt')
