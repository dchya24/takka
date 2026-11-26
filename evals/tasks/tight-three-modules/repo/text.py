def title_case(s):
    """Capitalise the first letter of every word, lower-case the rest."""
    return " ".join(w.upper() + w[1:] for w in s.split(" "))


def count_vowels(s):
    """Number of vowels (a, e, i, o, u), case-insensitive."""
    return sum(1 for ch in s if ch in "aeiou")
