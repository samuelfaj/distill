"""Normalizer for the folders_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'[A-Z]\d[A-Z]\d[A-Z]\d')


def normalize(value):
    code = re.sub(r'\s+', '', value).upper()
    return code if PATTERN.match(code) else None
