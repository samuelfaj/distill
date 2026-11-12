"""Normalizer for the returns_date column (rules in SPEC.md)."""
import datetime
import re

ORDER = 'mdy'


def normalize(value):
    value = value.strip()
    match = re.fullmatch(r'(\d{4})-(\d{2})-(\d{2})', value)
    if match:
        year, month, day = map(int, match.groups())
    else:
        match = re.fullmatch(r'(\d{1,2})/(\d{1,2})/(\d{4})', value)
        if not match:
            return None
        first, second, year = map(int, match.groups())
        day, month = (first, second) if ORDER == 'dmy' else (second, first)
    return f'{year:04}-{month:02}-{day:02}'
