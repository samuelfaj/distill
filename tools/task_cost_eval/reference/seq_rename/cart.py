from pricing import format_price


def line_text(name, cents, qty):
    return f"{qty} x {name}: {format_price(cents * qty)}"
