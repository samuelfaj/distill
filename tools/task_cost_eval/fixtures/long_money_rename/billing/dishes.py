from money import Money
from money import Money as M


def line_total_dishes(qty):
    price = Money(93, 'BRL')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def double_dishes(cents):
    return (M(cents) + M(cents)).amount_cents
