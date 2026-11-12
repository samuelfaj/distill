from money import Money
from money import Money as M


def line_total_dishes(qty):
    price = Money('BRL', 93)
    return Money(price.currency, price.cents * qty).to_dict()


def double_dishes(cents):
    return (M('USD', cents) + M('USD', cents)).cents
