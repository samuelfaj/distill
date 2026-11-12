import money
from money import Money as M


def wire_projects(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'projects'}


def double_projects(cents):
    return (M(cents) + M(cents)).amount_cents


def make_projects(cents):
    return money.Money(cents, 'USD').to_dict()
