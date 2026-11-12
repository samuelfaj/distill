from money import Money


def label_zones(cents):
    m = Money(currency='BRL', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def wire_zones(cents):
    return {'amount_cents': cents, 'currency': 'BRL', 'source': 'zones'}
