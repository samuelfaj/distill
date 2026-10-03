import unittest

from cart import add_item


class CartTest(unittest.TestCase):
    def test_adds_quantity(self):
        self.assertEqual(add_item({}, "pen", 2), {"pen": 2})


if __name__ == "__main__":
    unittest.main()
