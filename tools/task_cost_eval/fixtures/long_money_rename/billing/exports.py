from money import Money


def total_exports(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents


def roundtrip_exports(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 865, m.currency).to_dict()


def convert_exports(cents, currency):
    return Money(cents * 865 // 100, currency).to_dict()
