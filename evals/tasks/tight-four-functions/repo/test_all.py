import unittest

from chunks import chunk
from money import parse_money
from ranges import merge_ranges
from slug import slugify


class All(unittest.TestCase):
    def test_slug(self):
        self.assertEqual(slugify("  Hello, World!! "), "hello-world")
        self.assertEqual(slugify("a--b__c"), "a-b-c")

    def test_money(self):
        self.assertEqual(parse_money("$1,234.50"), 123450)
        self.assertEqual(parse_money("$0.05"), 5)

    def test_ranges(self):
        self.assertEqual(merge_ranges([(5, 8), (1, 3), (2, 4), (8, 10)]), [(1, 4), (5, 10)])
        self.assertEqual(merge_ranges([]), [])

    def test_chunk(self):
        self.assertEqual(chunk([1, 2, 3, 4, 5], 2), [[1, 2], [3, 4], [5]])
        with self.assertRaises(ValueError):
            chunk([1], 0)


if __name__ == "__main__":
    unittest.main()
