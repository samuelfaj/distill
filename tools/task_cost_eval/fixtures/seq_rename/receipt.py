from pricing import fmt_price


def receipt_total(cents_list):
    return "Total: " + fmt_price(sum(cents_list))
