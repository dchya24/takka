# Spec

- `slug.py`: `slugify(s)` lowercases, replaces runs of non-alphanumerics with a single "-", strips leading/trailing "-".
- `money.py`: `parse_money(s)` turns "$1,234.50" into the integer 123450 (cents).
- `ranges.py`: `merge_ranges(rs)` merges overlapping or touching (a, b) integer ranges and returns them sorted.
- `chunks.py`: `chunk(seq, n)` splits a list into lists of length n (the last may be shorter); n < 1 raises ValueError.
