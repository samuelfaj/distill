def report(orders, as_of=None):
    groups = {}
    for order in orders:
        if order['status'] != 'open':
            continue
        for item in order['items']:
            units = item['quantity'] - item['shipped']
            if units <= 0:
                continue
            row = groups.setdefault(item['sku'], {'sku': item['sku'], 'units': 0, 'order_ids': set()})
            row['units'] += units
            row['order_ids'].add(order['id'])
    return [{'sku': key, 'units': groups[key]['units'], 'order_ids': sorted(groups[key]['order_ids'])} for key in sorted(groups)]
