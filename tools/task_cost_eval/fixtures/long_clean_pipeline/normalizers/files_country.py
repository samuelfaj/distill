"""Normalizer for the files_country column (rules in SPEC.md)."""
ALIASES = {'spain': 'ES', 'espana': 'ES', 'italy': 'IT', 'sri lanka': 'LK'}


def normalize(value):
    key = value.strip().casefold()
    if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
        return key.upper()
    return ALIASES.get(key)
