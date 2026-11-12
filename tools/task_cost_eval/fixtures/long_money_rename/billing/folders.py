from money import Money
from money import Money as M


def line_total_folders(qty):
    price = Money(771, 'JPY')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def double_folders(cents):
    return (M(cents) + M(cents)).amount_cents
