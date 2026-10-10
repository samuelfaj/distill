from revenue import report as revenue
from backlog import report as backlog
from aging import report as aging


def totals(orders, as_of=None):
    return {'orders': len(orders), 'ordered_units': sum(item['quantity'] for order in orders for item in order['items'])}


REPORTS = {'totals': totals, 'revenue': revenue, 'backlog': backlog, 'aging': aging}
