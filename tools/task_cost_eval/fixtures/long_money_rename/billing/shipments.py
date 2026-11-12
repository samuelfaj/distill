from money import Money


def unit_price_shipments():
    return Money(787).to_dict()


def cheapest_shipments(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_shipments(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 787
