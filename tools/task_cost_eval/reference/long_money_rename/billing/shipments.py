from money import Money


def unit_price_shipments():
    return Money('USD', 787).to_dict()


def cheapest_shipments(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_shipments(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 787
