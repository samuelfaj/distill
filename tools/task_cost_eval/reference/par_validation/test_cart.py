import unittest

from cart import add_item


class CartTest(unittest.TestCase):
    def test_adds_quantity(self):
        self.assertEqual(add_item({}, "pen", 2), {"pen": 2})

    def test_rejects_empty_name(self):
        with self.assertRaises(ValueError):
            add_item({}, " ", 1)

    def test_rejects_non_positive_qty(self):
        with self.assertRaises(ValueError):
            add_item({}, "pen", 0)


if __name__ == "__main__":
    unittest.main()
