import unittest
from dates import is_leap, days_in_month


class DateTests(unittest.TestCase):
    def test_is_leap(self):
        self.assertTrue(is_leap(2000))
        self.assertFalse(is_leap(1900))
        self.assertTrue(is_leap(2024))
        self.assertFalse(is_leap(2023))

    def test_days_in_month(self):
        self.assertEqual(days_in_month(2000, 2), 29)
        self.assertEqual(days_in_month(1900, 2), 28)
        self.assertEqual(days_in_month(2024, 4), 30)
        self.assertEqual(days_in_month(2024, 12), 31)


if __name__ == "__main__":
    unittest.main()
