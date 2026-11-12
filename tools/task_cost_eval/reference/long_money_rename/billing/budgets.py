from money import Money


def field_budgets(cents):
    m = Money('USD', cents)
    return getattr(m, 'cents') + 686


def convert_budgets(cents, currency):
    return Money(currency, cents * 686 // 100).to_dict()


def discount_budgets(cents, pct):
    m = Money('USD', cents)
    m.cents -= m.cents * pct // 100
    return m.to_dict()
