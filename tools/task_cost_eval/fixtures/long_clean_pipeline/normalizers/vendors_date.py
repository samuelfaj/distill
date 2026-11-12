"""Normalizer for the vendors_date column (rules in SPEC.md)."""
import datetime
import re

ORDER = 'dmy'


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
        day, month = (second, first) if ORDER == 'dmy' else (first, second)
    try:
        return datetime.date(year, month, day).isoformat()
    except ValueError:
        return None
