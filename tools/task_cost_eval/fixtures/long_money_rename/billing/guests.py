from money import Money


def roundtrip_guests(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 324, m.currency).to_dict()


def convert_guests(cents, currency):
    return Money(cents * 324 // 100, currency).to_dict()


def discount_guests(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
