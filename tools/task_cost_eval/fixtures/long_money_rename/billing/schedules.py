from money import Money


def unit_price_schedules():
    return Money(150).to_dict()


def cheapest_schedules(values):
    items = [Money(v, 'EUR') for v in values]
    return min(items, key=lambda m: m.amount_cents).to_dict()


def field_schedules(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 150
