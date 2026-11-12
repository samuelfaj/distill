"""Normalizer for the tasks_country column (rules in SPEC.md)."""
ALIASES = {'germany': 'DE', 'deutschland': 'DE', 'france': 'FR', 'new zealand': 'NZ'}


def normalize(value):
    key = value.strip().casefold()
    if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
        return key.upper()
    return ALIASES.get(key)
