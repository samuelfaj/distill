from money import Money


def unit_price_orders():
    return Money('USD', 774).to_dict()


def line_total_orders(qty):
    price = Money('USD', 774)
    return Money(price.currency, price.cents * qty).to_dict()


def total_orders(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
