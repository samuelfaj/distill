"""Normalizer for the labels_country column (rules in SPEC.md)."""
ALIASES = {'mexico': 'MX', 'chile': 'CL', 'costa rica': 'CR', 'peru': 'PE'}


def normalize(value):
    key = ' '.join(value.split()).casefold()
    if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
        return key.upper()
    return ALIASES.get(key, value)
