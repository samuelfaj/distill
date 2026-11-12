import unittest

from query import filter_tasks


class QueryTests(unittest.TestCase):
    def test_open(self):
        tasks = [{'id': 1, 'title': 'a', 'done': False, 'created': '2026-01-01'},
                 {'id': 2, 'title': 'b', 'done': True, 'created': '2026-01-02'}]
        self.assertEqual([t['id'] for t in filter_tasks(tasks, 'open')], [1])
