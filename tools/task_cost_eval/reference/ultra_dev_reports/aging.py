from datetime import date


def report(orders, as_of=None):
    if as_of is None:
        raise ValueError('as_of is required')
    today = date.fromisoformat(as_of)
    rows = [{'bucket': label, 'orders': 0, 'units': 0} for label in ('0-7', '8-30', '31+')]
    for order in orders:
        if order['status'] != 'open':
            continue
        units = sum(item['quantity'] - item['shipped'] for item in order['items'])
        if not units:
            continue
        age = max(0, (today - date.fromisoformat(order['placed'])).days)
        bucket = 0 if age <= 7 else 1 if age <= 30 else 2
        rows[bucket]['orders'] += 1
        rows[bucket]['units'] += units
    return rows
