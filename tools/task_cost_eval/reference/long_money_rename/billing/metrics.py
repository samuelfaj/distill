from money import Money


def field_metrics(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 881


def convert_metrics(cents, currency):
    return Money(currency, cents * 881 // 100).to_dict()


def discount_metrics(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
