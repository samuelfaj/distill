from money import Money


def roundtrip_surveys(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 586).to_dict()


def convert_surveys(cents, currency):
    return Money(currency, cents * 586 // 100).to_dict()


def discount_surveys(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
