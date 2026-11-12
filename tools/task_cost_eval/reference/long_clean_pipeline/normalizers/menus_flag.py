"""Normalizer for the menus_flag column (rules in SPEC.md)."""
TRUE = ('sim', 's')
FALSE = ('nao', 'n')


def normalize(value):
    word = value.strip().casefold()
    if word in TRUE:
        return 'true'
    if word in FALSE:
        return 'false'
    return None
