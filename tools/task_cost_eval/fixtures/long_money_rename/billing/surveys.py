from money import Money


def roundtrip_surveys(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 586, m.currency).to_dict()


def convert_surveys(cents, currency):
    return Money(cents * 586 // 100, currency).to_dict()


def discount_surveys(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
