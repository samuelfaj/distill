import json
from errors import FeedError
from validation import normalize


def parse(text):
    result = []
    for row, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except ValueError as error:
            raise FeedError('syntax', row, 'jsonl') from error
        result.append(normalize(record, row))
    return result
