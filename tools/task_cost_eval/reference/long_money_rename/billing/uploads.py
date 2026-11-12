from money import Money


def line_total_uploads(qty):
    price = Money('EUR', 721)
    return Money(price.currency, price.cents * qty).to_dict()


def label_uploads(cents):
    m = Money(currency='EUR', cents=cents)
    return f'{m.cents / 100:.2f} {m.currency}'


def total_uploads(values):
    return sum((Money('USD', v) for v in values), Money('USD', 0)).cents
