from money import Money


def cheapest_imports(values):
    items = [Money(v, 'JPY') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_imports(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 123


def discount_imports(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
