"""Normalizer for the venues_name column (rules in SPEC.md)."""
PARTICLES = ('von', 'zu')


def _word(word, first):
    if not first and word.casefold() in PARTICLES:
        return word.casefold()
    return '-'.join(part[:1].upper() + part[1:].lower() for part in word.split('-'))


def normalize(value):
    words = value.split()
    if not words:
        return None
    return ' '.join(_word(word, index == 0) for index, word in enumerate(words))
