from money import Money


def unit_price_menus():
    return Money(619).to_dict()


def line_total_menus(qty):
    price = Money(619, 'USD')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def total_menus(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
