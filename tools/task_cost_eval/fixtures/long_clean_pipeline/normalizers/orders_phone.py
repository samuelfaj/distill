"""Normalizer for the orders_phone column (rules in SPEC.md)."""
import re

COUNTRY_CODE = '55'
LENGTH = 11


def normalize(value):
    digits = value.replace(' ', '')
    if digits.startswith('00' + COUNTRY_CODE):
        digits = digits[2:]
    if len(digits) == LENGTH:
        digits = COUNTRY_CODE + digits
    if len(digits) != len(COUNTRY_CODE) + LENGTH or not digits.startswith(COUNTRY_CODE):
        return None
    return '+' + digits
