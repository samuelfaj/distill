from money import Money


def roundtrip_labels(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 693).to_dict()


def convert_labels(cents, currency):
    return Money(currency, cents * 693 // 100).to_dict()


def discount_labels(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
