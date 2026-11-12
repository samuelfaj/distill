"""Normalizer for the imports_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'[A-Z]\d{4}[A-Z]{3}')


def normalize(value):
    code = re.sub(r'\s+', '', value).upper()
    return code if PATTERN.fullmatch(code) else None
