from money import Money


def unit_price_drivers():
    return Money('USD', 424).to_dict()


def cheapest_drivers(values):
    items = [Money('JPY', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_drivers(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 424
