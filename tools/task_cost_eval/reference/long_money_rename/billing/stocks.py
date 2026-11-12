from money import Money


def line_total_stocks(qty):
    price = Money('USD', 717)
    return Money(price.currency, price.cents * qty).to_dict()


def label_stocks(cents):
    m = Money(currency='USD', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def total_stocks(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
