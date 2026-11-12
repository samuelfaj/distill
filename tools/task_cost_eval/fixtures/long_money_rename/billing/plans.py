import money
from money import Money as M


def wire_plans(cents):
    return {'amount_cents': cents, 'currency': 'EUR', 'source': 'plans'}


def double_plans(cents):
    return (M(cents) + M(cents)).amount_cents


def make_plans(cents):
    return money.Money(cents, 'EUR').to_dict()
