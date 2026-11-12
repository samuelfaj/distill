from money import Money


def total_reports(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents


def roundtrip_reports(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 665, m.currency).to_dict()


def convert_reports(cents, currency):
    return Money(cents * 665 // 100, currency).to_dict()
