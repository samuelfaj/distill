import copy
from datetime import date


def normalize(orders):
    result = copy.deepcopy(orders)
    seen = set()
    for order in result:
        if not isinstance(order['id'], str) or order['id'] in seen:
            raise ValueError('duplicate or invalid order id')
        seen.add(order['id'])
        date.fromisoformat(order['placed'])
        if order['status'] not in ('open', 'shipped', 'cancelled'):
            raise ValueError('invalid status')
        for item in order['items']:
            for key in ('quantity', 'shipped', 'unit_cents'):
                if type(item[key]) is not int or item[key] < 0:
                    raise ValueError('invalid item quantity or price')
            if item['shipped'] > item['quantity']:
                raise ValueError('shipped exceeds quantity')
    return result
