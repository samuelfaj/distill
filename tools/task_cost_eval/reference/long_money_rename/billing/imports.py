from money import Money


def cheapest_imports(values):
    items = [Money('JPY', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_imports(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 123


def discount_imports(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
