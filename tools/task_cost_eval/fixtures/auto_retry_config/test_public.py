import unittest

from client import call


class ClientTests(unittest.TestCase):
    def test_success_needs_no_sleep(self):
        sleeps = []
        self.assertEqual(call('orders', lambda endpoint: endpoint.upper(), sleeps.append), 'ORDERS')
        self.assertEqual(sleeps, [])

    def test_gives_up_after_the_configured_attempts(self):
        calls = []

        def send(endpoint):
            calls.append(endpoint)
            raise ConnectionError('down')

        with self.assertRaises(ConnectionError):
            call('orders', send, lambda seconds: None)
        self.assertGreater(len(calls), 1)
