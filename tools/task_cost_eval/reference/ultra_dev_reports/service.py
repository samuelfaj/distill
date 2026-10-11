from orders import normalize
from registry import REPORTS


def render(name, orders, as_of=None):
    if name not in REPORTS:
        raise ValueError('unknown report: ' + name)
    return REPORTS[name](normalize(orders), as_of=as_of)
