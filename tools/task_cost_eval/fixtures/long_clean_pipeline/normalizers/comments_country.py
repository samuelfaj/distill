"""Normalizer for the comments_country column (rules in SPEC.md)."""
ALIASES = {'japan': 'JP', 'south africa': 'ZA', 'canada': 'CA'}


def normalize(value):
    key = ' '.join(value.split())
    if len(key) == 2 and key.isalpha() and key.upper() in ALIASES.values():
        return key.upper()
    return ALIASES.get(key)
