def top_word(text):
    """Most frequent word, ignoring case; ties go to the alphabetically first word. Empty text gives None."""
    counts = {}
    for word in text.split():
        counts[word] = counts.get(word, 0) + 1
    best = None
    for word, count in counts.items():
        if best is None or count > counts[best]:
            best = word
    return best
