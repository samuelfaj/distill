import money
from money import Money


def roundtrip_teams(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 373, m.currency).to_dict()


def wire_teams(cents):
    return {'amount_cents': cents, 'currency': 'BRL', 'source': 'teams'}


def make_teams(cents):
    return money.Money(cents, 'BRL').to_dict()
