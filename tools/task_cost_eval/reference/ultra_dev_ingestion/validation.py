import re
from errors import FeedError


def normalize(record, row):
    if not isinstance(record, dict):
        raise FeedError('record', row, '')
    result = {}
    for field in ('id', 'warehouse', 'sku'):
        value = record.get(field)
        if not isinstance(value, str) or not value.strip():
            raise FeedError('field', row, field)
        result[field] = value.strip()
    delta = record.get('delta')
    if isinstance(delta, str) and re.fullmatch(r'[+-]?[0-9]+', delta):
        delta = int(delta)
    if type(delta) is not int:
        raise FeedError('field', row, 'delta')
    result['delta'] = delta
    return result
