import unittest

from temperature import to_fahrenheit


class TemperatureTest(unittest.TestCase):
    def test_boiling_point(self):
        self.assertEqual(to_fahrenheit(100), 212)


if __name__ == "__main__":
    unittest.main()
