from money import Money


def total_exports(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents


def roundtrip_exports(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 865).to_dict()


def convert_exports(cents, currency):
    return Money(currency, cents * 865 // 100).to_dict()
