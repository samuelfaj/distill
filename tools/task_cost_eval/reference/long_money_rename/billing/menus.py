from money import Money


def unit_price_menus():
    return Money('USD', 619).to_dict()


def line_total_menus(qty):
    price = Money('USD', 619)
    return Money(price.currency, price.cents * qty).to_dict()


def total_menus(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
