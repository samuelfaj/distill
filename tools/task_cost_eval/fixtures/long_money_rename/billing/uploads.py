from money import Money


def line_total_uploads(qty):
    price = Money(721, 'EUR')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def label_uploads(cents):
    m = Money(amount_cents=cents, currency='EUR')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def total_uploads(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
