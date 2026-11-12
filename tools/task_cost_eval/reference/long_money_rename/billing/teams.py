import money
from money import Money


def roundtrip_teams(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 373).to_dict()


def wire_teams(cents):
    return {'amount_cents': cents, 'currency': 'BRL', 'source': 'teams'}


def make_teams(cents):
    return money.Money('BRL', cents).to_dict()
