from money import Money


def unit_price_receipts():
    return Money('USD', 237).to_dict()


def line_total_receipts(qty):
    price = Money('JPY', 237)
    return Money(price.currency, price.cents * qty).to_dict()


def total_receipts(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
