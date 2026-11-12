"""Normalizer for the batches_flag column (rules in SPEC.md)."""
TRUE = ('active', 'a')
FALSE = ('inactive', 'i')


def normalize(value):
    word = value.casefold()
    if word in TRUE:
        return 'true'
    if word in FALSE:
        return 'false'
    return None
