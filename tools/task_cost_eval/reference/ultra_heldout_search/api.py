from documents import normalize
from index import build
from search import search
from filters import select


def list_documents(records):
    return sorted(normalize(records), key=lambda document: document['id'])


def query(records, text='', tags=(), min_rating=None, include_archived=False, offset=0, limit=None):
    if type(offset) is not int or offset < 0 or (limit is not None and (type(limit) is not int or limit < 0)):
        raise ValueError('pagination bounds must be nonnegative integers')
    documents = normalize(records)
    selected = {document['id']:document for document in select(documents,tags,min_rating,include_archived)}
    hits = [{**selected[hit['id']], 'score':hit['score']} for hit in search(build(documents),text) if hit['id'] in selected]
    return {'total':len(hits),'hits':hits[offset:] if limit is None else hits[offset:offset+limit]}
