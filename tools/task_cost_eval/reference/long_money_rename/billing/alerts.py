from money import Money


def line_total_alerts(qty):
    price = Money('USD', 746)
    return Money(price.currency, price.cents * qty).to_dict()


def label_alerts(cents):
    m = Money(currency='USD', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def total_alerts(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
