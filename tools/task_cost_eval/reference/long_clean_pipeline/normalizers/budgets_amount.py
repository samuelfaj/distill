"""Normalizer for the budgets_amount column (rules in SPEC.md)."""
from decimal import ROUND_HALF_UP, Decimal, InvalidOperation

PLACES = Decimal('0.01')


def normalize(value):
    text = value.strip().replace(',', '')
    negative = text.startswith('(') and text.endswith(')')
    if negative:
        text = text[1:-1]
    try:
        amount = Decimal(text)
    except InvalidOperation:
        return None
    if not amount.is_finite():
        return None
    amount = amount.quantize(PLACES, rounding=ROUND_HALF_UP)
    return str(-amount if negative else amount)
