from money import Money


def cheapest_sessions(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_sessions(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 270


def discount_sessions(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
