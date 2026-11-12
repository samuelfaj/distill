import money
from money import Money as M


def wire_venues(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'venues'}


def double_venues(cents):
    return (M('USD', cents) + M('USD', cents)).cents


def make_venues(cents):
    return money.Money('USD', cents).to_dict()
