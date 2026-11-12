from money import Money


def roundtrip_labels(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 693, m.currency).to_dict()


def convert_labels(cents, currency):
    return Money(cents * 693 // 100, currency).to_dict()


def discount_labels(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
