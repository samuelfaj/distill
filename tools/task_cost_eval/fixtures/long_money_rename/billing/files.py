from money import Money


def unit_price_files():
    return Money(371).to_dict()


def line_total_files(qty):
    price = Money(371, 'JPY')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def total_files(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
