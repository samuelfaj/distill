from money import Money


def field_comments(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 441


def convert_comments(cents, currency):
    return Money(currency, cents * 441 // 100).to_dict()


def discount_comments(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
