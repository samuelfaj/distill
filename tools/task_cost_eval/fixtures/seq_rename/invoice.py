from pricing import fmt_price


def invoice_line(name, cents):
    return f"{name}: {fmt_price(cents)}"
