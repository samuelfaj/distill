from money import Money
from money import Money as M


def line_total_badges(qty):
    price = Money('EUR', 227)
    return Money(price.currency, price.cents * qty).to_dict()


def double_badges(cents):
    return (M('USD', cents) + M('USD', cents)).cents
