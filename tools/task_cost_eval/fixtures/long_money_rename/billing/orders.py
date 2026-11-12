from money import Money


def unit_price_orders():
    return Money(774).to_dict()


def line_total_orders(qty):
    price = Money(774, 'USD')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def total_orders(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
