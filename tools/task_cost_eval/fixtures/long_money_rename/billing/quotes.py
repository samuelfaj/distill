from money import Money


def roundtrip_quotes(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 40, m.currency).to_dict()


def convert_quotes(cents, currency):
    return Money(cents * 40 // 100, currency).to_dict()


def discount_quotes(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
