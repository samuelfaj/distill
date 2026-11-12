from money import Money


def label_zones(cents):
    m = Money(amount_cents=cents, currency='BRL')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def wire_zones(cents):
    return {'amount_cents': cents, 'currency': 'BRL', 'source': 'zones'}
