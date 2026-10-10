import json
import csv_feed
import jsonl_feed
from errors import FeedError
from validation import normalize
from aggregate import summarize


def load(format, text):
    if format == 'csv':
        return csv_feed.parse(text)
    if format == 'jsonl':
        return jsonl_feed.parse(text)
    if format != 'json':
        raise FeedError('format', 0, 'format')
    try:
        rows = json.loads(text)
    except ValueError as error:
        raise FeedError('syntax', 0, 'json') from error
    if not isinstance(rows, list):
        raise FeedError('record', 0, '')
    return [normalize(record, row) for row,record in enumerate(rows, 1)]


def combine(feeds):
    return summarize([record for format,text in feeds for record in load(format,text)])
