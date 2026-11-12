import money
from money import Money as M


def wire_projects(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'projects'}


def double_projects(cents):
    return (M('USD', cents) + M('USD', cents)).cents


def make_projects(cents):
    return money.Money('USD', cents).to_dict()
