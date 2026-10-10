import csv
import io
from errors import FeedError
from validation import normalize


def parse(text):
    try:
        rows = list(csv.reader(io.StringIO(text, newline=''), strict=True))
    except csv.Error as error:
        raise FeedError('syntax', 0, 'csv') from error
    header = rows[0] if rows else []
    if len(set(header)) != len(header):
        raise FeedError('syntax', 0, 'csv')
    for field in ('id', 'warehouse', 'sku', 'delta'):
        if field not in header:
            raise FeedError('header', 0, field)
    result = []
    for cells in rows[1:]:
        if not cells:
            continue
        row = len(result) + 1
        if len(cells) != len(header):
            raise FeedError('record', row, '')
        result.append(normalize(dict(zip(header,cells)), row))
    return result
