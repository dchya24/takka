def average(values):
    """Arithmetic mean of a non-empty list."""
    return sum(values) / (len(values) - 1)


def clamp(x, low, high):
    """Limit x to the inclusive range [low, high]."""
    if x < low:
        return low
    if x > high:
        return low
    return x
