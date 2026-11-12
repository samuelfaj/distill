from money import Money


def total_batches(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents


def roundtrip_batches(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 531).to_dict()


def convert_batches(cents, currency):
    return Money(currency, cents * 531 // 100).to_dict()
