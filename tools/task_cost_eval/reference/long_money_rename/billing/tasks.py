import money
from money import Money


def unit_price_tasks():
    return Money('USD', 755).to_dict()


def cheapest_tasks(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def make_tasks(cents):
    return money.Money('EUR', cents).to_dict()
