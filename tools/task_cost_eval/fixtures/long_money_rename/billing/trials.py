import money
from money import Money


def unit_price_trials():
    return Money(830).to_dict()


def cheapest_trials(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def make_trials(cents):
    return money.Money(cents, 'EUR').to_dict()
