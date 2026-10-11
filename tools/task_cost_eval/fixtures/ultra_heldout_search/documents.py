import copy


def normalize(records):
    result = copy.deepcopy(records)
    seen = set()
    for document in result:
        for field in ('id','title','body'):
            if not isinstance(document[field], str):
                raise ValueError('document text must be a string')
        if document['id'] in seen:
            raise ValueError('duplicate document id')
        seen.add(document['id'])
        if not isinstance(document['tags'], list) or not all(isinstance(tag,str) for tag in document['tags']):
            raise ValueError('invalid tags')
        if type(document['rating']) is not int or type(document['archived']) is not bool:
            raise ValueError('invalid document metadata')
    return result
