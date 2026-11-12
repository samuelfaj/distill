from money import Money


def unit_price_schedules():
    return Money('USD', 150).to_dict()


def cheapest_schedules(values):
    items = [Money('EUR', v) for v in values]
    return min(items, key=lambda m: m.cents).to_dict()


def field_schedules(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 150
