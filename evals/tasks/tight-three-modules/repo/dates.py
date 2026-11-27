def is_leap(year):
    """Gregorian leap year."""
    return year % 4 == 0 and year % 100 != 0


def days_in_month(year, month):
    """Number of days in the given month (1-12)."""
    if month == 2:
        return 29 if is_leap(year) else 28
    return 30 if month in (4, 6, 9, 11) else 31
