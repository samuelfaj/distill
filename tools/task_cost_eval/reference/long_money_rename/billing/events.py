from money import Money


def label_events(cents):
    m = Money(currency='USD', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def wire_events(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'events'}
