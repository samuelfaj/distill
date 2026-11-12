from money import Money


def roundtrip_quotes(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 40).to_dict()


def convert_quotes(cents, currency):
    return Money(currency, cents * 40 // 100).to_dict()


def discount_quotes(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
