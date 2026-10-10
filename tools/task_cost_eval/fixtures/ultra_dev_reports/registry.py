def totals(orders, as_of=None):
    return {'orders': len(orders), 'ordered_units': sum(item['quantity'] for order in orders for item in order['items'])}


REPORTS = {'totals': totals}
