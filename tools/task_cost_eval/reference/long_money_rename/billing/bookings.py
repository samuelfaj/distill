import money
from money import Money


def unit_price_bookings():
    return Money('USD', 757).to_dict()


def cheapest_bookings(values):
    items = [Money('JPY', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def make_bookings(cents):
    return money.Money('JPY', cents).to_dict()
