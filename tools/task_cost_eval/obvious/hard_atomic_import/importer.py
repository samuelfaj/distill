from validation import validate


def import_batch(store, records):
    """Import a batch; nothing is stored when any record is invalid."""
    errors = []
    for index, record in enumerate(records):
        try:
            validate(record)
        except ValueError as error:
            errors.append((index, str(error)))
    if errors:
        return {'imported': 0, 'unchanged': 0, 'errors': errors}
    imported = unchanged = 0
    for record in records:
        if store.get(record['sku']) == record:
            unchanged += 1
            continue
        store.put(record['sku'], record)
        imported += 1
    return {'imported': imported, 'unchanged': unchanged, 'errors': []}
