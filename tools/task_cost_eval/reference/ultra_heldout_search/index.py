from collections import Counter
from documents import normalize
from tokenizer import words


def build(records):
    result = {}
    for document in normalize(records):
        counts = Counter(words(document['body']))
        for token in words(document['title']):
            counts[token] += 2
        result[document['id']] = dict(counts)
    return result
