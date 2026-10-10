def convert(usd_cents, rate):
    return (2 * usd_cents * rate['numerator'] + rate['denominator']) // (2 * rate['denominator'])


def lines_for(order, catalog):
    lines = []
    for item in order['items']:
        quantity = item['quantity']
        if type(quantity) is not int or quantity <= 0:
            raise ValueError('quantity must be a positive integer')
        product = catalog.get(item['sku'])
        lines.append({'sku':item['sku'], 'quantity':quantity, 'usd_cents':quantity * product['cents']})
    return lines
