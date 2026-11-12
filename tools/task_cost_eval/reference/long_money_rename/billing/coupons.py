from money import Money


def cheapest_coupons(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_coupons(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 400


def discount_coupons(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
