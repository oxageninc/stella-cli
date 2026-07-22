import unittest

from slugger import slugify


class SlugifyTests(unittest.TestCase):
    def test_words_and_punctuation(self) -> None:
        self.assertEqual(slugify("Stella, Fleet Ready!"), "stella-fleet-ready")

    def test_outer_separators_are_removed(self) -> None:
        self.assertEqual(slugify(" --Terminal Bench-- "), "terminal-bench")

    def test_repeated_separators_collapse(self) -> None:
        self.assertEqual(slugify("one___two   three"), "one-two-three")


if __name__ == "__main__":
    unittest.main()
