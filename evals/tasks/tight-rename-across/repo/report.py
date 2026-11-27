from utils import fmt_price


def report_row(name, cents):
    return f"{name:<10}" + fmt_price(cents)
