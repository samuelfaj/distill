import unittest

from normalizers.menus_flag import normalize


class MenusFlagTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('SIM'), 'true')
        self.assertEqual(normalize(' nao '), 'false')
