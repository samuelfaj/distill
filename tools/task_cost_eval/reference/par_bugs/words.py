def top_word(text):
    counts = {}
    for word in text.lower().split():
        counts[word] = counts.get(word, 0) + 1
    if not counts:
        return None
    return min(counts, key=lambda word: (-counts[word], word))
