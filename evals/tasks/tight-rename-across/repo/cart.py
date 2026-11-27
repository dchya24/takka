from utils import fmt_price


def cart_line(item, cents):
    return f"{item}: " + fmt_price(cents)
