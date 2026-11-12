from money import Money


def label_vendors(cents):
    m = Money(currency='EUR', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def wire_vendors(cents):
    return {'amount_cents': cents, 'currency': 'EUR', 'source': 'vendors'}
