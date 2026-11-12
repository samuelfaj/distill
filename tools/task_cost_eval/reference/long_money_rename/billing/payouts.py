import money
from money import Money as M


def wire_payouts(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'payouts'}


def double_payouts(cents):
    return (M('USD', cents) + M('USD', cents)).cents


def make_payouts(cents):
    return money.Money('USD', cents).to_dict()
