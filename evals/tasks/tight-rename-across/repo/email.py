from utils import fmt_price


def email_subject(cents):
    return "Your total is " + fmt_price(cents)
