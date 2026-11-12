from money import Money


def field_rooms(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 614


def convert_rooms(cents, currency):
    return Money(cents * 614 // 100, currency).to_dict()


def discount_rooms(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
