from money import Money


def cheapest_sessions(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_sessions(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 270


def discount_sessions(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
