from money import Money


def unit_price_files():
    return Money('USD', 371).to_dict()


def line_total_files(qty):
    price = Money('JPY', 371)
    return Money(price.currency, price.cents * qty).to_dict()


def total_files(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
