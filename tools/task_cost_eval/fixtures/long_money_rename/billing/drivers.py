from money import Money


def unit_price_drivers():
    return Money(424).to_dict()


def cheapest_drivers(values):
    items = [Money(v, 'JPY') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_drivers(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 424
