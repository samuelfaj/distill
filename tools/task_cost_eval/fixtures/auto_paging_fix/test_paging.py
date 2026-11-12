import unittest

from paging import page, page_count


class PagingTests(unittest.TestCase):
    def test_first_page(self):
        self.assertEqual(page(list(range(10)), 1, 3), [0, 1, 2])

    def test_count(self):
        self.assertEqual(page_count(10, 3), 4)
