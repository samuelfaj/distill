def count_words(text):
    """Map each word to the number of times it appears."""
    counts = {}
    for word in text.split():
        counts[word] = counts.get(word, 0) + 1
    return counts
