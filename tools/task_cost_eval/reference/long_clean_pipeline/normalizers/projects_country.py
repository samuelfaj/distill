"""Normalizer for the projects_country column (rules in SPEC.md)."""
ALIASES = {'brazil': 'BR', 'brasil': 'BR', 'portugal': 'PT', 'united kingdom': 'GB'}


def normalize(value):
    key = ' '.join(value.split()).casefold()
    if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
        return key.upper()
    return ALIASES.get(key)
