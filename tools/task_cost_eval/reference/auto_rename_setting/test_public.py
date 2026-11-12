import unittest

import settings
from worker import describe


class SettingsTests(unittest.TestCase):
    def test_defaults(self):
        self.assertEqual(settings.load()['timeout_s'], 10)

    def test_describe(self):
        self.assertTrue(describe(settings.load()).startswith('retries='))
