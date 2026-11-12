"""Normalizer for the stocks_flag column (rules in SPEC.md)."""
TRUE = ('ja', 'j')
FALSE = ('nein', 'n')


def normalize(value):
    word = value.strip().casefold()
    if word in TRUE:
        return 'true'
    if word in FALSE:
        return 'false'
    return 'false'
