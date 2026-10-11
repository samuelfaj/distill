from tokenizer import words


def search(index, query):
    terms = set(words(query))
    hits = []
    for identifier, counts in index.items():
        if all(term in counts for term in terms):
            hits.append({'id':identifier,'score':sum(counts[term] for term in terms)})
    return sorted(hits, key=lambda hit: (-hit['score'], hit['id']))
