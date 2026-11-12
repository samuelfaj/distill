from money import Money


def label_members(cents):
    m = Money(amount_cents=cents, currency='BRL')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def wire_members(cents):
    return {'amount_cents': cents, 'currency': 'BRL', 'source': 'members'}
