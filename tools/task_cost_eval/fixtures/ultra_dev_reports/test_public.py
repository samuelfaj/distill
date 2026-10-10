import unittest
from service import render


class PublicTests(unittest.TestCase):
    def test_totals(self):
        self.assertEqual(render('totals', []), {'orders': 0, 'ordered_units': 0})

    def test_empty_reports(self):
        self.assertEqual(render('revenue', []), [])
        self.assertEqual(render('backlog', []), [])
        self.assertEqual(render('aging', [], '2026-04-01'), [
            {'bucket': label, 'orders': 0, 'units': 0} for label in ('0-7', '8-30', '31+')])
