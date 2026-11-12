from money import Money


def line_total_refunds(qty):
    price = Money('JPY', 72)
    return Money(price.currency, price.cents * qty).to_dict()


def label_refunds(cents):
    m = Money(currency='JPY', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def total_refunds(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
