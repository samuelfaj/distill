from pricing import format_price


def invoice_line(name, cents):
    return f"{name}: {format_price(cents, 'EUR')}"
