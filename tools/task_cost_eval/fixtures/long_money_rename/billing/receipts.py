from money import Money


def unit_price_receipts():
    return Money(237).to_dict()


def line_total_receipts(qty):
    price = Money(237, 'JPY')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def total_receipts(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
