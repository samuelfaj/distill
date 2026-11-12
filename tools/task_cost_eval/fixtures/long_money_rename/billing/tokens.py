from money import Money


def unit_price_tokens():
    return Money(36).to_dict()


def cheapest_tokens(values):
    items = [Money(v, 'USD') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_tokens(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 36
