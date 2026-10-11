import unittest
from api import default_config, resolve
from export import render


class ConfigTests(unittest.TestCase):
    def test_defaults(self):
        self.assertEqual(default_config()['server']['port'], 8080)

    def test_precedence(self):
        config = resolve(['{"server":{"port":9000},"features":{"tags":["dev"]}}'], {'APP__SERVER__PORT':'9100'})
        self.assertEqual(config['server']['port'], 9100)
        self.assertEqual(config['features']['tags'], ['dev'])
        self.assertIn('APP__SERVER__PORT=9100\n', render(config,'env'))
