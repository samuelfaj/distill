from money import Money
from money import Money as M


def line_total_invoices(qty):
    price = Money('BRL', 717)
    return Money(price.currency, price.cents * qty).to_dict()


def double_invoices(cents):
    return (M('USD', cents) + M('USD', cents)).cents
