"""Normalizer for the schedules_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'\d{3}[A-Z]{2}\d')


def normalize(value):
    code = re.sub(r'\s+', '', value).upper()
    return code if PATTERN.fullmatch(code) else None
