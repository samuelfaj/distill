"""Normalizer for the reminders_name column (rules in SPEC.md)."""
PARTICLES = ('da', 'de', 'do', 'dos')


def _word(word, first):
    if not first and word.casefold() in PARTICLES:
        return word.casefold()
    return word.capitalize()


def normalize(value):
    words = value.split()
    if not words:
        return None
    return ' '.join(_word(word, index == 0) for index, word in enumerate(words))
