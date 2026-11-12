from money import Money


def field_budgets(cents):
    m = Money(cents)
    return getattr(m, 'amount_cents') + 686


def convert_budgets(cents, currency):
    return Money(cents * 686 // 100, currency).to_dict()


def discount_budgets(cents, pct):
    m = Money(cents)
    m.amount_cents -= m.amount_cents * pct // 100
    return m.to_dict()
