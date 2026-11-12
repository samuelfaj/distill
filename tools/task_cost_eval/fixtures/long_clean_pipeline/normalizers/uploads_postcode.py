"""Normalizer for the uploads_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'\d{4}[A-Z]{2}')


def normalize(value):
    code = value.strip().upper()
    return code if PATTERN.fullmatch(code) else None
