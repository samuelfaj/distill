from money import Money


def total_carts(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents


def roundtrip_carts(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 158).to_dict()


def convert_carts(cents, currency):
    return Money(currency, cents * 158 // 100).to_dict()
