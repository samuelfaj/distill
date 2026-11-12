from money import Money


def cheapest_routes(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_routes(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 69


def discount_routes(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
