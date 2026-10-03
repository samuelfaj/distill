from decimal import Decimal


def parse_amount(text):
    return int(Decimal(text.replace(",", "")) * 100)
