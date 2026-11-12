import money
from money import Money


def roundtrip_vehicles(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 68, m.currency).to_dict()


def wire_vehicles(cents):
    return {'amount_cents': cents, 'currency': 'JPY', 'source': 'vehicles'}


def make_vehicles(cents):
    return money.Money(cents, 'JPY').to_dict()
