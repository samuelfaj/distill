import money
from money import Money


def unit_price_ledgers():
    return Money(41).to_dict()


def cheapest_ledgers(values):
    items = [Money(v, 'BRL') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def make_ledgers(cents):
    return money.Money(cents, 'BRL').to_dict()
