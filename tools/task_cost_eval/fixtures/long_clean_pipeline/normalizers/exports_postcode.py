"""Normalizer for the exports_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'[A-Z]{1,2}\d[A-Z\d]?\d[A-Z]{2}')


def normalize(value):
    code = re.sub(r'\s+', '', value)
    return code if PATTERN.fullmatch(code) else None
