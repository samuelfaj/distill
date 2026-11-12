from money import Money
from money import Money as M


def line_total_tickets(qty):
    price = Money(785, 'BRL')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def double_tickets(cents):
    return (M(cents) + M(cents)).amount_cents
