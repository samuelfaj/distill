import money
from money import Money


def roundtrip_returns(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 512, m.currency).to_dict()


def wire_returns(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'returns'}


def make_returns(cents):
    return money.Money(cents, 'USD').to_dict()
