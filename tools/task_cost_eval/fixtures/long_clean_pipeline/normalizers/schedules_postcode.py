"""Normalizer for the schedules_postcode column (rules in SPEC.md)."""
import re

PATTERN = re.compile(r'\d{3}[A-Z]{2}\d')


def normalize(value):
    code = value.strip().upper()
    return code if PATTERN.fullmatch(code) else None
