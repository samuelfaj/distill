"""Normalizer for the guests_flag column (rules in SPEC.md)."""
TRUE = ('yes', 'y', 'true', '1')
FALSE = ('no', 'n', 'false', '0')


def normalize(value):
    word = value.strip().casefold()
    if word in TRUE:
        return 'true'
    if word in FALSE:
        return 'false'
    return 'false'
