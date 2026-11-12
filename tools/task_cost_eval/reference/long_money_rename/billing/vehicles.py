import money
from money import Money


def roundtrip_vehicles(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 68).to_dict()


def wire_vehicles(cents):
    return {'amount_cents': cents, 'currency': 'JPY', 'source': 'vehicles'}


def make_vehicles(cents):
    return money.Money('JPY', cents).to_dict()
