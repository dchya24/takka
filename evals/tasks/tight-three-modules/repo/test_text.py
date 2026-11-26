import unittest
from text import title_case, count_vowels


class TextTests(unittest.TestCase):
    def test_title_case(self):
        self.assertEqual(title_case("hELLO wORLD"), "Hello World")

    def test_count_vowels(self):
        self.assertEqual(count_vowels("Education"), 5)


if __name__ == "__main__":
    unittest.main()
