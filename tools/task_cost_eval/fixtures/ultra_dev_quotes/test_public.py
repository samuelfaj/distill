import unittest
from catalog import Catalog
from rates import RateTable
from service import QuoteService
from invoice import build


class CheckoutTests(unittest.TestCase):
    def setUp(self):
        self.catalog = Catalog({'pen': {'name':'Pen','cents':101,'tags':[]}})
        self.service = QuoteService(self.catalog, RateTable({'USD':{'numerator':1,'denominator':1}}))
        self.order = {'id':'order-1','items':[{'sku':'pen','quantity':2}]}

    def test_price_refresh(self):
        self.assertEqual(self.service.quote(self.order)['total_cents'], 202)
        self.catalog.set_price('pen', 250)
        self.assertEqual(self.service.quote(self.order)['total_cents'], 500)

    def test_invoice_snapshot(self):
        build(self.service, self.order, note='paid')
        self.assertNotIn('invoice_note', self.service.quote(self.order))
