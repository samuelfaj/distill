from money import Money


def field_comments(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 441


def convert_comments(cents, currency):
    return Money(cents * 441 // 100, currency).to_dict()


def discount_comments(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
