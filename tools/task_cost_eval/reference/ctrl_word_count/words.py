import string


def count_words(text):
    """Map each word to the number of times it appears, ignoring case and surrounding punctuation."""
    counts = {}
    for raw in text.split():
        word = raw.strip(string.punctuation).lower()
        if word:
            counts[word] = counts.get(word, 0) + 1
    return counts
