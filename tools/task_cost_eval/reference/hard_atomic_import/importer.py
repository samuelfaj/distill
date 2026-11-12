from store import StoreFull
from validation import validate

_ABSENT = object()


def import_batch(store, records):
    """Import a batch all or nothing.

    Returns {'imported', 'unchanged', 'errors'}; errors lists (index, message) for every invalid
    record, and then nothing is written. If the store refuses a record, every write of this
    batch is undone before StoreFull reaches the caller.
    """
    errors = []
    for index, record in enumerate(records):
        try:
            validate(record)
        except ValueError as error:
            errors.append((index, str(error)))
    if errors:
        return {'imported': 0, 'unchanged': 0, 'errors': errors}
    previous, imported, unchanged = {}, 0, 0
    try:
        for record in records:
            key = record['sku']
            current = store.get(key)
            if current == record:
                unchanged += 1
                continue
            previous.setdefault(key, _ABSENT if current is None else current)
            store.put(key, record)
            imported += 1
    except StoreFull:
        for key, value in previous.items():
            if value is _ABSENT:
                store.delete(key)
        for key, value in previous.items():
            if value is not _ABSENT:
                store.put(key, value)
        raise
    return {'imported': imported, 'unchanged': unchanged, 'errors': []}
