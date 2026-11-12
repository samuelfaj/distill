from money import Money


def unit_price_answers():
    return Money(136).to_dict()


def line_total_answers(qty):
    price = Money(136, 'JPY')
    return Money(price.amount_cents * qty, price.currency).to_dict()


def total_answers(values):
    return sum((Money(v) for v in values), Money(0)).amount_cents
