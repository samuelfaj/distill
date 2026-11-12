import unittest

from report import render_text, summarize


class ReportTests(unittest.TestCase):
    def test_text(self):
        rows = [{'region': 'West', 'amount_cents': 1250}]
        self.assertEqual(render_text(summarize(rows)), 'West: $12.50 in 1 orders')
