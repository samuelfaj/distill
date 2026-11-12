import unittest

from normalizers.batches_flag import normalize


class BatchesFlagTests(unittest.TestCase):
    def test_examples(self):
        self.assertEqual(normalize('ACTIVE'), 'true')
        self.assertEqual(normalize(' inactive '), 'false')
