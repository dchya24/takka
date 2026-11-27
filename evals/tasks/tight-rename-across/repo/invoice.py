from utils import fmt_price


def invoice_total(cents):
    return "Total " + fmt_price(cents)
