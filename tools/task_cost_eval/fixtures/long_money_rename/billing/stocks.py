from money import Money


def line_total_stocks(qty):
    price = Money(717, 'USD')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def label_stocks(cents):
    m = Money(amount_cents=cents, currency='USD')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def total_stocks(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
