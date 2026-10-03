from pricing import format_price


def receipt_total(cents_list):
    return "Total: " + format_price(sum(cents_list))
