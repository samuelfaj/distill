import money
from money import Money


def unit_price_ledgers():
    return Money('USD', 41).to_dict()


def cheapest_ledgers(values):
    items = [Money('BRL', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def make_ledgers(cents):
    return money.Money('BRL', cents).to_dict()
