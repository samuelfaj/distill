import money
from money import Money


def unit_price_tasks():
    return Money(755).to_dict()


def cheapest_tasks(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def make_tasks(cents):
    return money.Money(cents, 'EUR').to_dict()
