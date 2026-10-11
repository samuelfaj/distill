def report(orders, as_of=None):
    groups = {}
    for order in orders:
        if order['status'] == 'cancelled':
            continue
        for item in order['items']:
            if not item['shipped']:
                continue
            row = groups.setdefault(order['region'], {'region': order['region'], 'units': 0, 'revenue_cents': 0})
            row['units'] += item['shipped']
            row['revenue_cents'] += item['shipped'] * item['unit_cents']
    return [groups[key] for key in sorted(groups)]
