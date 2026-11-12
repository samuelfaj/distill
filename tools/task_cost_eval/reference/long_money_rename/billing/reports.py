from money import Money


def total_reports(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents


def roundtrip_reports(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 665).to_dict()


def convert_reports(cents, currency):
    return Money(currency, cents * 665 // 100).to_dict()
