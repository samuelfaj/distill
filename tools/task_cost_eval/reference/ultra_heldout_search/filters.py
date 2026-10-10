from documents import normalize


def select(records, tags=(), min_rating=None, include_archived=False):
    if min_rating is not None and type(min_rating) is not int:
        raise ValueError('min_rating must be an integer')
    requested = set(tags)
    return sorted([document for document in normalize(records)
                   if (include_archived or not document['archived'])
                   and requested.issubset(document['tags'])
                   and (min_rating is None or document['rating'] >= min_rating)], key=lambda document: document['id'])
