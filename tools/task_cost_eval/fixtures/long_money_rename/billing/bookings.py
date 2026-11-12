import money
from money import Money


def unit_price_bookings():
    return Money(757).to_dict()


def cheapest_bookings(values):
    items = [Money(v, 'JPY') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def make_bookings(cents):
    return money.Money(cents, 'JPY').to_dict()
