from money import Money


def total_batches(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents


def roundtrip_batches(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 531, m.currency).to_dict()


def convert_batches(cents, currency):
    return Money(cents * 531 // 100, currency).to_dict()
