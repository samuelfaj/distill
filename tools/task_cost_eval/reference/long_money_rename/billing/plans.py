import money
from money import Money as M


def wire_plans(cents):
    return {'amount_cents': cents, 'currency': 'EUR', 'source': 'plans'}


def double_plans(cents):
    return (M('USD', cents) + M('USD', cents)).cents


def make_plans(cents):
    return money.Money('EUR', cents).to_dict()
