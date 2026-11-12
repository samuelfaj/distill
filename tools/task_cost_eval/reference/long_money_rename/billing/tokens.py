from money import Money


def unit_price_tokens():
    return Money('USD', 36).to_dict()


def cheapest_tokens(values):
    items = [Money('USD', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_tokens(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 36
