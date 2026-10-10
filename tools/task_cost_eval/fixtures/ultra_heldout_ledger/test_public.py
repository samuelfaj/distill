import unittest
from state import Ledger
from projector import apply
from accounts import snapshot


class DeliveryTests(unittest.TestCase):
    def test_retry_after_later_event(self):
        ledger = Ledger({'a':10,'b':0})
        transfer = {'id':'t','seq':1,'kind':'transfer','source':'a','target':'b','amount':3}
        self.assertTrue(apply(ledger,transfer))
        apply(ledger,{'id':'c','seq':2,'kind':'credit','account':'a','amount':1})
        self.assertFalse(apply(ledger,transfer))
        self.assertEqual(snapshot(ledger)['balances'], {'a':8,'b':3})

    def test_failed_transfer_atomic(self):
        ledger = Ledger({'a':1,'b':0})
        before = snapshot(ledger)
        with self.assertRaises(ValueError):
            apply(ledger,{'id':'t','seq':1,'kind':'transfer','source':'a','target':'b','amount':9})
        self.assertEqual(snapshot(ledger), before)
        self.assertEqual(snapshot(ledger)['balances'], {'a':1,'b':0})
