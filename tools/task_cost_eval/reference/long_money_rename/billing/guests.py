from money import Money


def roundtrip_guests(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 324).to_dict()


def convert_guests(cents, currency):
    return Money(currency, cents * 324 // 100).to_dict()


def discount_guests(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
