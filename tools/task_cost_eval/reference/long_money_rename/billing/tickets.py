from money import Money
from money import Money as M


def line_total_tickets(qty):
    price = Money('BRL', 785)
    return Money(price.currency, price.cents * qty).to_dict()


def double_tickets(cents):
    return (M('USD', cents) + M('USD', cents)).cents
