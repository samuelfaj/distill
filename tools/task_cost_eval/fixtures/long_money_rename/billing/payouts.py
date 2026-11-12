import money
from money import Money as M


def wire_payouts(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'payouts'}


def double_payouts(cents):
    return (M(cents) + M(cents)).amount_cents


def make_payouts(cents):
    return money.Money(cents, 'USD').to_dict()
