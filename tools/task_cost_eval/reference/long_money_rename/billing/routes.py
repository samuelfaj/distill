from money import Money


def cheapest_routes(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_routes(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 69


def discount_routes(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
