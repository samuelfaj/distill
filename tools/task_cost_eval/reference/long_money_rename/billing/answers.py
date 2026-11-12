from money import Money


def unit_price_answers():
    return Money('USD', 136).to_dict()


def line_total_answers(qty):
    price = Money('JPY', 136)
    return Money(price.currency, price.cents * qty).to_dict()


def total_answers(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
