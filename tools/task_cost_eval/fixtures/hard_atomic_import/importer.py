from validation import validate


def import_batch(store, records):
    """Store each record under its sku and return how many were stored."""
    count = 0
    for record in records:
        validate(record)
        store.put(record['sku'], record)
        count += 1
    return count
