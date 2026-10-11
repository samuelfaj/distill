import unittest
from api import combine, load
from errors import FeedError


class FeedTests(unittest.TestCase):
    def test_legacy_json(self):
        self.assertEqual(combine([('json','[{"id":"a","warehouse":"W","sku":"S","delta":3}]')]),
                         [{'warehouse':'W','sku':'S','delta':3}])
        with self.assertRaises(FeedError):
            load('json', '{}')

    def test_formats(self):
        record = {'id':'a','warehouse':'W','sku':'S','delta':3}
        self.assertEqual(load('csv','id,warehouse,sku,delta\na,W,S,+3\n'), [record])
        self.assertEqual(load('jsonl','\n{"id":"a","warehouse":"W","sku":"S","delta":3}\n'), [record])
