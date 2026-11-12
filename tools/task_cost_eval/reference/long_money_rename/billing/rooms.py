from money import Money


def field_rooms(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 614


def convert_rooms(cents, currency):
    return Money(currency, cents * 614 // 100).to_dict()


def discount_rooms(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
