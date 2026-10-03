import unittest

from temperature import to_fahrenheit


class TemperatureTest(unittest.TestCase):
    def test_boiling_point(self):
        self.assertEqual(to_fahrenheit(100), 212)

    def test_rejects_non_number(self):
        with self.assertRaises(TypeError):
            to_fahrenheit("100")

    def test_rejects_below_absolute_zero(self):
        with self.assertRaises(ValueError):
            to_fahrenheit(-300)


if __name__ == "__main__":
    unittest.main()
