from pricing import fmt_price


def line_text(name, cents, qty):
    return f"{qty} x {name}: {fmt_price(cents * qty)}"
