import money
from money import Money


def unit_price_trials():
    return Money('USD', 830).to_dict()


def cheapest_trials(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def make_trials(cents):
    return money.Money('EUR', cents).to_dict()
