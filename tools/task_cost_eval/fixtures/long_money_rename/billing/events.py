from money import Money


def label_events(cents):
    m = Money(amount_cents=cents, currency='USD')
    return f'{m.amount_cents / 100:.2f} {m.currency}'


def wire_events(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'events'}
