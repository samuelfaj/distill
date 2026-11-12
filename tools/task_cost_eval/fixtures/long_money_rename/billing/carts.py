from money import Money


def total_carts(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents


def roundtrip_carts(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 158, m.currency).to_dict()


def convert_carts(cents, currency):
    return Money(cents * 158 // 100, currency).to_dict()
