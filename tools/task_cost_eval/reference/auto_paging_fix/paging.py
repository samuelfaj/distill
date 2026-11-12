def page(items, number, size):
    """Return page `number` (1-based) of `items`, `size` items per page."""
    if number < 1 or size < 1:
        raise ValueError('page number and size start at 1')
    start = (number - 1) * size
    return list(items[start:start + size])


def page_count(total, size):
    return -(-total // size)
