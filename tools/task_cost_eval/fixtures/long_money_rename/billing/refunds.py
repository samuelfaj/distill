from money import Money


def line_total_refunds(qty):
    price = Money(72, 'JPY')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def label_refunds(cents):
    m = Money(amount_cents=cents, currency='JPY')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def total_refunds(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
