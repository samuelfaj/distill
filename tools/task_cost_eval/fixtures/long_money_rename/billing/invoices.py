from money import Money
from money import Money as M


def line_total_invoices(qty):
    price = Money(717, 'BRL')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def double_invoices(cents):
    return (M(cents) + M(cents)).amount_cents
