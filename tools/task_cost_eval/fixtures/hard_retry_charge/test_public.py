import unittest

from client import pay
from gateway import CardDeclined, FakeGateway


class PayTests(unittest.TestCase):
    def test_retries_a_timeout(self):
        gateway = FakeGateway(['timeout_before', 'ok'])
        self.assertEqual(pay(gateway, 'o1', 500), 'ch_1')
        self.assertEqual(gateway.calls, 2)

    def test_lost_response_is_not_charged_twice(self):
        gateway = FakeGateway(['timeout_after', 'ok'])
        pay(gateway, 'o1', 500)
        self.assertEqual(len(gateway.charges), 1)

    def test_gives_up_after_three_attempts(self):
        gateway = FakeGateway(['timeout_before'] * 5)
        with self.assertRaises(TimeoutError):
            pay(gateway, 'o1', 500)
        self.assertEqual(gateway.calls, 3)

    def test_declined_is_not_retried(self):
        gateway = FakeGateway(declined=['o1'])
        with self.assertRaises(CardDeclined):
            pay(gateway, 'o1', 500)
        self.assertEqual(gateway.calls, 1)
