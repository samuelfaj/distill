"""Normalizer for the dishes_flag column (rules in SPEC.md)."""
TRUE = ('on', 'enabled')
FALSE = ('off', 'disabled')


def normalize(value):
    word = value.strip().casefold()
    if word in TRUE:
        return 'true'
    if word in FALSE:
        return 'false'
    return None
