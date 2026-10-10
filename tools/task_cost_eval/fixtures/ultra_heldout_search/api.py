from documents import normalize


def list_documents(records):
    return sorted(normalize(records), key=lambda document: document['id'])


def query(records, text='', tags=(), min_rating=None, include_archived=False, offset=0, limit=None):
    raise NotImplementedError('search integration pending')
