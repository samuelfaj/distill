from money import Money
from money import Money as M


def line_total_badges(qty):
    price = Money(227, 'EUR')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def double_badges(cents):
    return (M(cents) + M(cents)).amount_cents
