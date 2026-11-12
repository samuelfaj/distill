from money import Money


def field_metrics(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 881


def convert_metrics(cents, currency):
    return Money(cents * 881 // 100, currency).to_dict()


def discount_metrics(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
